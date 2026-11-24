use crate::identity::{self, ms, oid};
use crate::ingest::indexeddb::TeamsCache;
use crate::ingest::{render, SourceError};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::LazyLock;

pub const RENDER_VERSION: i64 = 3;

static AMS_IMG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<img[^>]+itemtype="http://schema\.skype\.com/AMSImage"[^>]*>"#).unwrap());
static SRC_ATTR_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"src="([^"]+)""#).unwrap());

pub const EXTRA_STORES: &[(&str, &str)] = &[
    ("call-history-manager", "call-history"),
    ("calendar", "calendar"),
    ("activity-manager", "feed-items"),
    ("streams-replychain-manager", "saved"),
    ("streams-replychain-manager", "drafts"),
    ("streams-replychain-manager", "mentions"),
    ("streams-replychain-manager", "threads"),
    ("channel-info-pane-manager", "pinnedchannelmessages"),
    ("chat-info-pane-manager", "pinned-messages-store"),
    ("consumption-horizon-manager", "consumption-horizon-history"),
    ("p2p-shared-manager", "p2p-shared"),
    ("meeting-recap-manager", "meeting-recap-catchup"),
    ("files-mru-manager", "files_mru"),
];

#[derive(Debug, Clone)]
pub struct Person {
    pub mri: String,
    pub name: String,
    pub email: String,
    pub job_title: String,
    pub department: String,
    pub kind: String,
    pub raw: Value,
}

#[derive(Debug, Clone)]
pub struct Conversation {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub name_is_fallback: bool,
    pub topic: String,
    pub team_id: Option<String>,
    pub members: Vec<String>,
    pub last_message_at: Option<i64>,
    pub raw: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    pub kind: String,
    pub name: String,
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct Message {
    pub conversation_id: String,
    pub message_id: String,
    pub thread_id: Option<String>,
    pub parent_id: Option<String>,
    pub version: i64,
    pub arrived_at: Option<i64>,
    pub edited_at: Option<i64>,
    pub deleted_at: Option<i64>,
    pub sender_mri: String,
    pub sender_name: String,
    pub message_type: String,
    pub kind: String,
    pub content_html: String,
    pub text: String,
    pub subject: String,
    pub importance: String,
    pub reactions: Vec<Value>,
    pub attachments: Vec<Attachment>,
    pub properties: Value,
    pub raw: Value,
}

#[derive(Debug, Clone)]
pub struct RawRecord {
    pub source: String,
    pub key: String,
    pub data: Value,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub me_mri: Option<String>,
    pub people: HashMap<String, Person>,
    pub conversations: HashMap<String, Conversation>,
    pub messages: Vec<Message>,
    pub stats: BTreeMap<String, i64>,
    pub records: Vec<RawRecord>,
}

pub fn volatile_fields(source: &str) -> &'static [&'static str] {
    match source {
        "message" => &[
            "clientArrivalTime",
            "source",
            "prioritizeImDisplayName",
            "isConversationLastMessage",
            "isConversationLastMessageSanitized",
            "sendPipelineStatus",
            "sendPipelineOAMState",
            "pendingEmotionOps",
            "properties.isread",
            "properties.views",
        ],
        "conversation" => &[
            "version",
            "clientUpdateTime",
            "clientArrivalTime",
            "source",
            "chatTitle",
            "messages",
            "lastMessage",
            "lastNonTMMessage",
            "lastL1Message",
            "lastMessageTimeUtc",
            "nonFilteredLastMessageTimeUtc",
            "lastContentMessageTime",
            "lastUpdatedMessageId",
            "lastUpdatedMessageVersion",
            "lastL2MessageIdNotFromSelf",
            "lastRcMetadataVersion",
            "memberProperties",
            "oldConsumptionHorizonKeys",
            "keysWithLies",
            "invalidatedBySync",
            "invalidationDetails",
            "mruEvictedTime",
            "lastSeenSkipRosterState",
            "isPending",
            "localClientId",
            "cacheDeleteHistory",
            "threadVersion",
            "rosterVersion",
            "detailsVersion",
            "properties.consumptionhorizon",
            "properties.consumptionHorizonBookmark",
            "properties.rclch",
            "properties.rclchBookmark",
            "properties.lastimreceivedtime",
            "properties.lastimportantimreceivedtime",
            "properties.lasturgentimreceivedtime",
            "properties.consumptionHorizonForPinnedMessages",
            "properties.hasMessageDraft",
            "properties.isemptyconversation",
            "threadProperties.isRead",
            "threadProperties.isLastMessageFromMe",
            "threadProperties.version",
        ],
        "person" => &["metadata", "$$updated"],
        "activity-manager/feed-items" => &["isRead", "version"],
        "consumption-horizon-manager/consumption-horizon-history" => &["expiryTime"],
        _ => &[],
    }
}

pub fn stable_view(source: &str, data: &Value) -> Value {
    let volatile = volatile_fields(source);
    if volatile.is_empty() {
        return data.clone();
    }
    let Some(obj) = data.as_object() else {
        return data.clone();
    };
    let mut out = Map::new();
    for (k, v) in obj {
        if volatile.contains(&k.as_str()) {
            continue;
        }
        let nested_pfx = format!("{k}.");
        if let Some(sub) = v.as_object() {
            if volatile.iter().any(|f| f.starts_with(&nested_pfx)) {
                let mut sub_out = Map::new();
                for (kk, vv) in sub {
                    let full = format!("{k}.{kk}");
                    if !volatile.contains(&full.as_str()) {
                        sub_out.insert(kk.clone(), vv.clone());
                    }
                }
                out.insert(k.clone(), Value::Object(sub_out));
                continue;
            }
        }
        out.insert(k.clone(), v.clone());
    }
    Value::Object(out)
}

fn key_part(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
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
        other => other.to_string(),
    }
}

pub fn format_record_key(key: &Value) -> String {
    match key {
        Value::String(s) => s.clone(),
        Value::Array(arr) => arr.iter().map(key_part).collect::<Vec<_>>().join("/"),
        other => key_part(other),
    }
}

fn get_str(obj: &Map<String, Value>, key: &str) -> String {
    obj.get(key).and_then(|v| v.as_str()).unwrap_or("").to_string()
}

fn maybe_json(v: Option<&Value>) -> Value {
    match v {
        Some(Value::String(s)) if s.starts_with('[') || s.starts_with('{') => {
            serde_json::from_str(s).unwrap_or(Value::Null)
        }
        Some(other) => other.clone(),
        None => Value::Null,
    }
}

pub fn render_text(mtype: &str, content: &str, props: &Value, name_of: &dyn Fn(&str) -> String) -> (String, String) {
    let mut mentions = HashMap::new();
    if let Value::Array(arr) = maybe_json(props.get("mentions")) {
        for m in arr {
            if let Some(mo) = m.as_object() {
                if let Some(id_v) = mo.get("itemid").filter(|v| !v.is_null()) {
                    let id_s = match id_v {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    mentions.insert(id_s, get_str(mo, "mri"));
                }
            }
        }
    }
    if let Some(act) = props.get("activity").filter(|v| v.is_object()) {
        if content.trim().is_empty() {
            return ("event".to_string(), render::render_activity(act));
        }
    }
    let mut text = match mtype {
        "RichText/Html" | "Text" | "RichText" => render::html_to_text(content, &mentions),
        "RichText/Media_Card" => render::render_card(content),
        _ => {
            return (
                "event".to_string(),
                render::render_event(mtype, content, name_of).unwrap_or_default(),
            );
        }
    };
    if let Value::Array(files) = maybe_json(props.get("files")) {
        let mut labels = Vec::new();
        for f in files {
            if let Some(fname) = f.get("fileName").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
                let tag = format!("[file: {fname}]");
                if !text.contains(&tag) {
                    labels.push(tag);
                }
            }
        }
        if !labels.is_empty() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&labels.join(" "));
        }
    }
    ("message".to_string(), text)
}

fn extract_reactions(props: &Value, name_of: &dyn Fn(&str) -> String) -> Vec<Value> {
    let mut out = Vec::new();
    if let Value::Array(arr) = maybe_json(props.get("emotions")) {
        for e in arr {
            let Some(eo) = e.as_object() else { continue };
            let users: Vec<String> = eo
                .get("users")
                .and_then(|v| v.as_array())
                .map(|us| {
                    us.iter()
                        .filter_map(|u| u.as_object().map(|uo| get_str(uo, "mri")))
                        .collect()
                })
                .unwrap_or_default();
            if !users.is_empty() {
                let key = get_str(eo, "key");
                let names: Vec<String> = users.iter().map(|u| name_of(u)).collect();
                out.push(json!({
                    "key": key,
                    "emoji": render::reaction_emoji(&key),
                    "count": users.len(),
                    "users": names,
                }));
            }
        }
    }
    out
}

fn extract_attachments(props: &Value, content: &str) -> Vec<Attachment> {
    let mut out = Vec::new();
    let mut file_urls = HashSet::new();
    if let Value::Array(files) = maybe_json(props.get("files")) {
        for f in files {
            let Some(fo) = f.as_object() else { continue };
            let info = fo.get("fileInfo").and_then(|v| v.as_object());
            let url = info
                .and_then(|i| {
                    let s = get_str(i, "shareUrl");
                    if !s.is_empty() {
                        Some(s)
                    } else {
                        let fu = get_str(i, "fileUrl");
                        if !fu.is_empty() {
                            Some(fu)
                        } else {
                            None
                        }
                    }
                })
                .unwrap_or_else(|| get_str(fo, "objectUrl"));
            let name = {
                let n = get_str(fo, "fileName");
                if !n.is_empty() {
                    n
                } else {
                    let t = get_str(fo, "title");
                    if !t.is_empty() {
                        t
                    } else {
                        url.clone()
                    }
                }
            };
            file_urls.insert(url.clone());
            out.push(Attachment {
                kind: "file".to_string(),
                name,
                url,
            });
        }
    }
    if let Value::Array(links) = maybe_json(props.get("links")) {
        for l in links {
            let Some(lo) = l.as_object() else { continue };
            let url = get_str(lo, "url");
            if !url.is_empty() && !file_urls.contains(&url) {
                let title = get_str(lo, "title");
                out.push(Attachment {
                    kind: "link".to_string(),
                    name: if title.is_empty() { url.clone() } else { title },
                    url,
                });
            }
        }
    }
    for m in AMS_IMG_RE.find_iter(content) {
        if let Some(cap) = SRC_ATTR_RE.captures(m.as_str()) {
            out.push(Attachment {
                kind: "image".to_string(),
                name: "image".to_string(),
                url: render::html_unescape(&cap[1]),
            });
        }
    }
    out
}

fn kind_by_type(t: &str) -> String {
    match t {
        "Chat" => "chat".into(),
        "Topic" => "channel".into(),
        "Meeting" => "meeting".into(),
        "Space" => "team".into(),
        "EngageCommunity" => "community".into(),
        "Thread" | "StreamOfNotes" => "system".into(),
        "" => "chat".into(),
        other => other.to_ascii_lowercase(),
    }
}

pub fn derive_name(
    cid: &str,
    members: &[String],
    name_of: &dyn Fn(&str) -> String,
    me_mri: Option<&str>,
) -> (String, bool) {
    let owned_members: Vec<String>;
    let mlist: &[String] = if members.is_empty() {
        if let Some(cap) = identity::ONE_ON_ONE_RE.captures(cid) {
            owned_members = vec![format!("8:orgid:{}", &cap[1]), format!("8:orgid:{}", &cap[2])];
            &owned_members
        } else {
            &[]
        }
    } else {
        members
    };
    let is_me = |mri: &str| identity::same_person(Some(mri), me_mri);
    let mut names: Vec<String> = Vec::new();
    for mri in mlist {
        if is_me(mri) {
            continue;
        }
        let n = name_of(mri);
        if !n.is_empty() && !n.starts_with("8:") && !n.starts_with("28:") && !names.contains(&n) {
            names.push(n);
        }
    }
    if !names.is_empty() {
        names.sort();
        let shown: Vec<&str> = names.iter().take(5).map(String::as_str).collect();
        let mut s = shown.join(", ");
        if names.len() > 5 {
            s.push_str(&format!(" +{}", names.len() - 5));
        }
        return (s, false);
    }
    if !mlist.is_empty() {
        if let Some(me) = me_mri {
            if mlist.iter().all(|x| is_me(x)) {
                return (format!("{} (you)", name_of(me)), false);
            }
        }
    }
    (cid.to_string(), true)
}

fn content_dedup_hash(s: &str) -> [u8; 16] {
    let digest = Sha256::digest(s.as_bytes());
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

pub enum ExtractError<E> {
    Source(SourceError),
    Sink(E),
}

pub trait ExtractSink {
    type Error;
    fn on_me(&mut self, me_mri: &str) -> Result<(), Self::Error>;
    fn on_person(&mut self, person: Person) -> Result<(), Self::Error>;
    fn on_conversation(&mut self, conv: Conversation) -> Result<(), Self::Error>;
    fn on_message(&mut self, msg: Message) -> Result<(), Self::Error>;
    fn on_record(&mut self, rec: RawRecord) -> Result<(), Self::Error>;
}

struct SnapshotCollector {
    me_mri: Option<String>,
    people: HashMap<String, Person>,
    conversations: HashMap<String, Conversation>,
    messages: Vec<Message>,
    records: Vec<RawRecord>,
}

impl ExtractSink for SnapshotCollector {
    type Error = std::convert::Infallible;

    fn on_me(&mut self, me_mri: &str) -> Result<(), Self::Error> {
        self.me_mri = Some(me_mri.to_string());
        Ok(())
    }

    fn on_person(&mut self, person: Person) -> Result<(), Self::Error> {
        self.people.insert(person.mri.clone(), person);
        Ok(())
    }

    fn on_conversation(&mut self, conv: Conversation) -> Result<(), Self::Error> {
        self.conversations.insert(conv.id.clone(), conv);
        Ok(())
    }

    fn on_message(&mut self, msg: Message) -> Result<(), Self::Error> {
        self.messages.push(msg);
        Ok(())
    }

    fn on_record(&mut self, rec: RawRecord) -> Result<(), Self::Error> {
        self.records.push(rec);
        Ok(())
    }
}

pub fn extract(cache: &mut TeamsCache) -> Result<Snapshot, SourceError> {
    let mut collector = SnapshotCollector {
        me_mri: None,
        people: HashMap::new(),
        conversations: HashMap::new(),
        messages: Vec::new(),
        records: Vec::new(),
    };
    let stats = match extract_into(cache, &mut collector) {
        Ok(st) => st,
        Err(ExtractError::Source(e)) => return Err(e),
        Err(ExtractError::Sink(never)) => match never {},
    };
    Ok(Snapshot {
        me_mri: collector.me_mri,
        people: collector.people,
        conversations: collector.conversations,
        messages: collector.messages,
        stats,
        records: collector.records,
    })
}

pub fn extract_into<S: ExtractSink>(
    cache: &mut TeamsCache,
    sink: &mut S,
) -> Result<BTreeMap<String, i64>, ExtractError<S::Error>> {
    const CORE_STORES: &[(&str, &str)] = &[
        ("profiles", "profiles"),
        ("replychain-manager", "replychains-2"),
        ("replychain-manager", "replychains"),
        ("conversation-manager", "conversations"),
    ];
    cache.retain_wanted_stores(CORE_STORES, EXTRA_STORES);

    let me_mri = cache.user_object_id().map(|o| format!("8:orgid:{o}"));
    if let Some(ref me) = me_mri {
        sink.on_me(me).map_err(ExtractError::Sink)?;
    }
    let has_replychain_db = cache.find_database("replychain-manager").is_some();

    let mut by_oid: HashMap<String, String> = HashMap::new();
    let mut by_mri: HashMap<String, String> = HashMap::new();
    let mut seen_people: HashSet<String> = HashSet::new();
    let mut sink_err: Option<S::Error> = None;

    cache.for_each_taken_record("profiles", "profiles", |v| {
        if sink_err.is_some() {
            return;
        }
        let Some(po) = v.as_object() else { return };
        let mri = {
            let m = get_str(po, "mri");
            if !m.is_empty() {
                m
            } else {
                get_str(po, "objectId")
            }
        };
        if mri.is_empty() {
            return;
        }
        if let Err(e) = sink.on_record(RawRecord {
            source: "person".to_string(),
            key: mri.clone(),
            data: v.clone(),
        }) {
            sink_err = Some(e);
            return;
        }
        let email_raw = ["email", "$$email", "userPrincipalName"]
            .iter()
            .map(|k| get_str(po, k))
            .find(|s| !s.is_empty())
            .unwrap_or_default();
        let email = if email_raw.starts_with("8:") || email_raw.starts_with("28:") {
            String::new()
        } else {
            email_raw
        };
        let kind_raw = get_str(po, "type").to_ascii_lowercase();
        let display_name = get_str(po, "displayName");
        if !display_name.is_empty() {
            let o = oid(&mri);
            if !o.is_empty() {
                by_oid.insert(o, display_name.clone());
            }
            by_mri.insert(mri.clone(), display_name.clone());
        }
        seen_people.insert(mri.clone());
        if let Err(e) = sink.on_person(Person {
            mri,
            name: display_name,
            email,
            job_title: get_str(po, "jobTitle"),
            department: get_str(po, "department"),
            kind: if kind_raw.is_empty() {
                "person".to_string()
            } else {
                kind_raw
            },
            raw: v,
        }) {
            sink_err = Some(e);
        }
    });
    if let Some(e) = sink_err {
        return Err(ExtractError::Sink(e));
    }

    let name_of = |mri: &str| -> String {
        if let Some(n) = by_mri.get(mri) {
            return n.clone();
        }
        if let Some(n) = by_oid.get(&oid(mri)) {
            return n.clone();
        }
        mri.to_string()
    };

    let mut conv_raw: HashMap<String, Value> = HashMap::new();
    cache.for_each_taken_record("conversation-manager", "conversations", |c| {
        if sink_err.is_some() {
            return;
        }
        let Some(co) = c.as_object() else { return };
        let cid = get_str(co, "id");
        if cid.is_empty() {
            return;
        }
        if let Err(e) = sink.on_record(RawRecord {
            source: "conversation".to_string(),
            key: cid.clone(),
            data: c.clone(),
        }) {
            sink_err = Some(e);
            return;
        }
        let ver = co.get("version").and_then(ms).unwrap_or(0);
        let replace = match conv_raw.get(&cid) {
            None => true,
            Some(cur) => ver >= cur.get("version").and_then(ms).unwrap_or(0),
        };
        if replace {
            conv_raw.insert(cid, c);
        }
    });
    if let Some(e) = sink_err {
        return Err(ExtractError::Sink(e));
    }

    let team_names: HashMap<String, String> = conv_raw
        .iter()
        .filter_map(|(cid, tv)| {
            let ttp = tv.get("threadProperties").and_then(|v| v.as_object())?;
            let s = get_str(ttp, "spaceThreadTopic");
            let name = if !s.is_empty() { s } else { get_str(ttp, "topic") };
            (!name.is_empty()).then(|| (cid.clone(), name))
        })
        .collect();

    let mut seen_convs: HashSet<String> = HashSet::with_capacity(conv_raw.len());
    for (cid, c) in conv_raw {
        let (kind, topic, team_id, members, last_message_at) = {
            let co = c.as_object().unwrap();
            let tp = co.get("threadProperties").and_then(|v| v.as_object());
            let kind = if cid.starts_with("48:") {
                "system".to_string()
            } else {
                kind_by_type(&get_str(co, "type"))
            };
            let topic = tp
                .map(|t| {
                    let s = get_str(t, "topic");
                    if !s.is_empty() {
                        s
                    } else {
                        get_str(t, "spaceThreadTopic")
                    }
                })
                .unwrap_or_default();
            let team_id = {
                let t = get_str(co, "teamId");
                if t.is_empty() {
                    None
                } else {
                    Some(t)
                }
            };
            let members: Vec<String> = co
                .get("members")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|m| m.as_object().map(|mo| get_str(mo, "id")).filter(|s| !s.is_empty()))
                        .collect()
                })
                .unwrap_or_default();
            let last_message_at = co.get("lastMessageTimeUtc").and_then(ms).or_else(|| {
                co.get("lastMessage")
                    .and_then(|v| v.get("originalArrivalTime"))
                    .and_then(ms)
            });
            (kind, topic, team_id, members, last_message_at)
        };
        let mut name = topic.clone();
        let mut fallback = false;
        if kind == "channel" {
            let team_name = team_id
                .as_deref()
                .and_then(|tid| team_names.get(tid))
                .map(String::as_str)
                .unwrap_or("");
            let channel = if !topic.is_empty() {
                topic.clone()
            } else if team_id.as_deref() == Some(cid.as_str()) {
                "General".to_string()
            } else {
                String::new()
            };
            name = if !team_name.is_empty() {
                format!("{team_name} > {channel}")
            } else {
                channel
            };
        }
        if name.is_empty() {
            let (n, fb) = derive_name(&cid, &members, &name_of, me_mri.as_deref());
            name = n;
            fallback = fb;
        }
        seen_convs.insert(cid.clone());
        sink.on_conversation(Conversation {
            id: cid,
            kind,
            name,
            name_is_fallback: fallback,
            topic,
            team_id,
            members,
            last_message_at,
            raw: c,
        })
        .map_err(ExtractError::Sink)?;
    }
    drop(team_names);

    let mut seen_msgs: HashSet<(String, String, i64, [u8; 16])> = HashSet::new();
    let mut unique_msgs: HashSet<(String, String)> = HashSet::new();
    let mut chains: i64 = 0;

    for store in ["replychains-2", "replychains"] {
        cache.for_each_taken_record("replychain-manager", store, |rc| {
            if sink_err.is_some() {
                return;
            }
            let Value::Object(mut rco) = rc else { return };
            chains += 1;
            let rc_conv = get_str(&rco, "conversationId");
            let rc_thread = {
                let t = get_str(&rco, "replyChainId");
                if t.is_empty() {
                    None
                } else {
                    Some(t)
                }
            };
            let Some(Value::Object(mm)) = rco.remove("messageMap") else {
                return;
            };
            drop(rco);
            for (_, mval) in mm {
                if sink_err.is_some() {
                    return;
                }
                let Value::Object(mut mo) = mval else { continue };
                let conv = {
                    let c = get_str(&mo, "conversationId");
                    if !c.is_empty() {
                        c
                    } else {
                        rc_conv.clone()
                    }
                };
                let mid = {
                    let s = get_str(&mo, "id");
                    if !s.is_empty() {
                        s
                    } else {
                        mo.get("id").and_then(identity::clean_id).unwrap_or_default()
                    }
                };
                if conv.is_empty() || mid.is_empty() {
                    continue;
                }
                let content = get_str(&mo, "content");
                let version = mo.get("version").and_then(ms).unwrap_or(0);
                if !seen_msgs.insert((conv.clone(), mid.clone(), version, content_dedup_hash(&content))) {
                    continue;
                }
                unique_msgs.insert((conv.clone(), mid.clone()));
                if !seen_convs.contains(&conv) {
                    let (name, fallback) = derive_name(&conv, &[], &name_of, me_mri.as_deref());
                    seen_convs.insert(conv.clone());
                    if let Err(e) = sink.on_conversation(Conversation {
                        id: conv.clone(),
                        kind: if conv.starts_with("48:") {
                            "system".to_string()
                        } else {
                            "chat".to_string()
                        },
                        name,
                        name_is_fallback: fallback,
                        topic: String::new(),
                        team_id: None,
                        members: Vec::new(),
                        last_message_at: None,
                        raw: Value::Object(Map::new()),
                    }) {
                        sink_err = Some(e);
                        return;
                    }
                }
                if let Err(e) = sink.on_record(RawRecord {
                    source: "message".to_string(),
                    key: format!("{conv}/{mid}"),
                    data: Value::Object(mo.clone()),
                }) {
                    sink_err = Some(e);
                    return;
                }
                let props = match mo.remove("properties") {
                    Some(v) if v.is_object() => v,
                    _ => Value::Object(Map::new()),
                };
                mo.remove("content");
                let mtype = {
                    let t = get_str(&mo, "messageType");
                    if t.is_empty() {
                        "Unknown".to_string()
                    } else {
                        t
                    }
                };
                let sender_mri = get_str(&mo, "creator");
                let sender_name = {
                    let s1 = get_str(&mo, "imDisplayName");
                    if !s1.is_empty() {
                        s1
                    } else {
                        let s2 = get_str(&mo, "fromDisplayNameInToken");
                        if !s2.is_empty() {
                            s2
                        } else if !sender_mri.is_empty() {
                            name_of(&sender_mri)
                        } else {
                            String::new()
                        }
                    }
                };
                let deleted_at = props.get("deletetime").and_then(ms).or_else(|| {
                    if mo
                        .get("deletionInfo")
                        .map(|v| !v.is_null() && v != &Value::Bool(false))
                        .unwrap_or(false)
                    {
                        mo.get("clientArrivalTime").and_then(ms)
                    } else {
                        None
                    }
                });
                let (kind, text) = render_text(&mtype, &content, &props, &name_of);
                let arrived_at = mo
                    .get("originalArrivalTime")
                    .and_then(ms)
                    .or_else(|| mo.get("clientArrivalTime").and_then(ms));
                let edited_at = props.get("edittime").and_then(ms);
                let parent_id = {
                    let p = get_str(&mo, "parentMessageId");
                    if p.is_empty() {
                        None
                    } else {
                        Some(p)
                    }
                };
                let subject = props.as_object().map(|p| get_str(p, "subject")).unwrap_or_default();
                let importance = props.as_object().map(|p| get_str(p, "importance")).unwrap_or_default();
                let reactions = extract_reactions(&props, &name_of);
                let attachments = extract_attachments(&props, &content);

                if let Err(e) = sink.on_message(Message {
                    conversation_id: conv,
                    message_id: mid,
                    thread_id: rc_thread.clone(),
                    parent_id,
                    version,
                    arrived_at,
                    edited_at,
                    deleted_at,
                    sender_mri,
                    sender_name,
                    message_type: mtype,
                    kind,
                    content_html: content,
                    text,
                    subject,
                    importance,
                    reactions,
                    attachments,
                    properties: props,
                    raw: Value::Object(mo),
                }) {
                    sink_err = Some(e);
                    return;
                }
            }
        });
        if let Some(e) = sink_err {
            return Err(ExtractError::Sink(e));
        }
    }
    drop(seen_msgs);

    if !has_replychain_db || unique_msgs.is_empty() {
        return Err(ExtractError::Source(SourceError(
            "No messages found in the Teams cache. Teams may have changed its storage format; \
             run `teams raw stores` to inspect it."
                .to_string(),
        )));
    }

    for &(manager, store) in EXTRA_STORES {
        let src_name = format!("{manager}/{store}");
        cache.for_each_taken_keyed_record(manager, store, |k, v| {
            if sink_err.is_some() || v.is_null() {
                return;
            }
            if let Err(e) = sink.on_record(RawRecord {
                source: src_name.clone(),
                key: format_record_key(&k),
                data: v,
            }) {
                sink_err = Some(e);
            }
        });
        if let Some(e) = sink_err {
            return Err(ExtractError::Sink(e));
        }
    }

    let mut stats = BTreeMap::new();
    stats.insert("cache_people".to_string(), seen_people.len() as i64);
    stats.insert("cache_conversations".to_string(), seen_convs.len() as i64);
    stats.insert("cache_messages".to_string(), unique_msgs.len() as i64);
    stats.insert("cache_bad_records".to_string(), cache.bad_records as i64);
    stats.insert("cache_reply_chains".to_string(), chains);

    Ok(stats)
}
