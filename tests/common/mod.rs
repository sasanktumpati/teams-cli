#![allow(dead_code)]

use rusqlite::Connection;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, MutexGuard};
use teams_cli::ingest::extract::{Conversation, Message, Person, RawRecord, Snapshot};
use teams_cli::storage::archive;

pub const ME_OID: &str = "aaaaaaaa-0000-0000-0000-000000000001";
pub const ADA_OID: &str = "bbbbbbbb-0000-0000-0000-000000000002";
pub const BOB_OID: &str = "cccccccc-0000-0000-0000-000000000003";
pub const ME: &str = "8:orgid:aaaaaaaa-0000-0000-0000-000000000001";
pub const ADA: &str = "8:orgid:bbbbbbbb-0000-0000-0000-000000000002";
pub const BOB: &str = "8:orgid:cccccccc-0000-0000-0000-000000000003";
pub const DM_BOB: &str = "19:cccccccc-0000-0000-0000-000000000003_aaaaaaaa-0000-0000-0000-000000000001@unq.gbl.spaces";
pub const GROUP: &str = "19:group@thread.v2";
pub const MEETING_CHAT: &str = "19:meeting_abc@thread.v2";
pub const MID: &str = "1795000000003";
pub const EDITED: &str = "1795000000004";

pub const MENTIONS: &str = "streams-replychain-manager/mentions";
pub const FEED: &str = "activity-manager/feed-items";
pub const CALLS: &str = "call-history-manager/call-history";
pub const CALENDAR: &str = "calendar/calendar";
pub const SAVED: &str = "streams-replychain-manager/saved";
pub const PINNED_CHANNEL: &str = "channel-info-pane-manager/pinnedchannelmessages";

pub const HOUR: i64 = 3_600_000;

pub static NOW: LazyLock<i64> = LazyLock::new(archive::now_ms);

static ENV_LOCK: Mutex<()> = Mutex::new(());

pub fn env_lock() -> MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

pub fn t(hours_ago: f64) -> i64 {
    (*NOW as f64 - hours_ago * HOUR as f64) as i64
}

pub fn iso_utc(ms: i64) -> String {
    let ts = jiff::Timestamp::from_millisecond(ms).unwrap();
    let micros = (ts.subsec_nanosecond() / 1_000) as u32;
    format!("{}{:06}1Z", ts.strftime("%Y-%m-%dT%H:%M:%S."), micros)
}

pub fn msg(conv: &str, mid: &str, sender: &str, html: &str, arrived: i64, name: &str) -> Message {
    Message {
        conversation_id: conv.into(),
        message_id: mid.into(),
        thread_id: Some(mid.into()),
        parent_id: Some(mid.into()),
        version: 1,
        arrived_at: Some(arrived),
        edited_at: None,
        deleted_at: None,
        sender_mri: sender.into(),
        sender_name: name.into(),
        message_type: "RichText/Html".into(),
        kind: "message".into(),
        content_html: html.into(),
        text: html.replace("<p>", "").replace("</p>", ""),
        subject: String::new(),
        importance: String::new(),
        reactions: Vec::new(),
        attachments: Vec::new(),
        properties: json!({}),
        raw: json!({"id": mid}),
    }
}

pub fn conv(cid: &str, kind: &str, name: &str) -> Conversation {
    Conversation {
        id: cid.into(),
        kind: kind.into(),
        name: name.into(),
        name_is_fallback: false,
        topic: String::new(),
        team_id: None,
        members: Vec::new(),
        last_message_at: None,
        raw: json!({"id": cid, "properties": {"consumptionhorizon": ""},
                    "threadProperties": {"isRead": null, "isLastMessageFromMe": null}}),
    }
}

fn person(mri: &str, name: &str, email: &str) -> Person {
    Person {
        mri: mri.into(),
        name: name.into(),
        email: email.into(),
        job_title: String::new(),
        department: String::new(),
        kind: String::new(),
        raw: json!({}),
    }
}

pub fn snapshot(messages: Vec<Message>, conversations: Vec<Conversation>, records: Vec<RawRecord>) -> Snapshot {
    let people: HashMap<String, Person> = [
        person(ME, "Me Myself", "me@corp.test"),
        person(ADA, "Ada Lovelace", "ada@corp.test"),
        person(BOB, "Bob Builder", "bob@corp.test"),
    ]
    .into_iter()
    .map(|p| (p.mri.clone(), p))
    .collect();
    Snapshot {
        me_mri: Some(ME.into()),
        people,
        conversations: conversations.into_iter().map(|c| (c.id.clone(), c)).collect(),
        messages,
        stats: BTreeMap::new(),
        records,
    }
}

fn rec(source: &str, key: &str, data: Value) -> RawRecord {
    RawRecord {
        source: source.into(),
        key: key.into(),
        data,
    }
}

pub fn mention_record(mid: &str, chat: &str, sender: &str, sender_name: &str, ts: i64) -> RawRecord {
    let midf: f64 = mid.parse().unwrap();
    rec(
        MENTIONS,
        &format!("48:mentions/{mid}"),
        json!({
            "conversationId": "48:mentions",
            "originalArrivalTime": ts as f64,
            "messageMap": {
                format!("k{mid}"): {
                    "id": format!("9{mid}"),
                    "messageType": "RichText/Html",
                    "content": "<p>hey <span>Me</span></p>",
                    "originalArrivalTime": ts as f64,
                    "properties": {
                        "mentions": json!([{"itemid": "0", "mri": ME}]).to_string(),
                        "activity": {
                            "activityType": "mention",
                            "sourceThreadId": chat,
                            "sourceMessageId": midf,
                            "sourceReplyChainId": midf,
                            "sourceUserId": sender,
                            "sourceUserImDisplayName": sender_name,
                            "targetUserId": ME,
                            "activityTimestamp": ts as f64,
                        },
                    },
                },
            },
        }),
    )
}

pub fn feed_item(aid: &str, kind: &str, chat: &str, mid: &str, ts: i64, is_read: bool) -> RawRecord {
    let midf: f64 = mid.parse().unwrap();
    rec(
        FEED,
        aid,
        json!({
            "activityId": aid, "activityType": kind, "activitySubtype": null,
            "sourceThreadId": chat, "sourceMessageId": midf, "sourceReplyChainId": midf,
            "timestamp": ts as f64, "isRead": is_read, "messageId": midf,
        }),
    )
}

pub struct Call<'a> {
    pub state: &'a str,
    pub direction: &'a str,
    pub dur: Option<i64>,
    pub chat: Option<&'a str>,
    pub originator: (&'a str, Option<&'a str>),
    pub target: (&'a str, Option<&'a str>),
}

impl Default for Call<'_> {
    fn default() -> Self {
        Call {
            state: "Accepted",
            direction: "Outgoing",
            dur: Some(125_400),
            chat: None,
            originator: (ME, Some("Me Myself")),
            target: (ADA, None),
        }
    }
}

pub fn call_record(cid: &str, start: i64, c: Call) -> RawRecord {
    rec(
        CALLS,
        cid,
        json!({
            "id": cid, "callId": cid,
            "startTime": iso_utc(start),
            "connectTime": if c.state == "Accepted" { Value::from(iso_utc(start + 2000)) } else { Value::Null },
            "endTime": c.dur.map(|d| Value::from(iso_utc(start + d))).unwrap_or(Value::Null),
            "originalArrivalTime": start as f64,
            "durationInMs": c.dur,
            "callDirection": c.direction, "callType": "TwoParty", "callState": c.state,
            "originatorParticipant": {"id": c.originator.0, "displayName": c.originator.1},
            "targetParticipant": {"id": c.target.0, "displayName": c.target.1},
            "participantList": [],
            "groupChatThreadId": c.chat,
            "isCurrentUserPartOfCall": true,
        }),
    )
}

pub fn event(oid: &str, start: f64, end: f64, subject: &str, extra: Value) -> RawRecord {
    let mut data = json!({
        "objectId": oid, "subject": subject, "startTime": start, "endTime": end,
        "organizerName": "Ada Lovelace", "organizerAddress": "ada@corp.test",
        "location": "Microsoft Teams Meeting", "isOnlineMeeting": true,
        "shortOnlineMeetingJoinUrl": format!("https://teams.test/join/{oid}"), "myResponseType": "Accepted",
        "showAs": "Busy", "eventType": "SingleInstance", "bodyPreview": "  Agenda:\n  1. plans\n",
        "attendees": [
            {"name": "Me Myself", "address": "me@corp.test", "type": "Required", "role": "Attendee",
             "status": {"response": "Accepted"}},
            {"name": "Bob Builder", "address": "bob@corp.test", "type": "Optional", "role": "Attendee",
             "status": {"response": "None"}},
        ],
    });
    if let (Some(d), Some(x)) = (data.as_object_mut(), extra.as_object()) {
        for (k, v) in x {
            d.insert(k.clone(), v.clone());
        }
    }
    rec(CALENDAR, oid, data)
}

pub fn saved_record(key: &str, chat: &str, mid: &str, saved_at: i64) -> RawRecord {
    let midf: f64 = mid.parse().unwrap();
    rec(
        SAVED,
        key,
        json!({
            "conversationId": "48:saved",
            "originalArrivalTime": saved_at as f64,
            "messageMap": {key: {"id": key, "messageType": "RichText/Html", "content": "",
                                 "originalArrivalTime": saved_at as f64,
                                 "properties": {"activity": {
                                     "activityType": "savedMessage", "sourceThreadId": chat,
                                     "sourceMessageId": midf, "sourceReplyChainId": midf,
                                     "activityTimestamp": saved_at as f64}}}},
        }),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn pinned_message(
    source: &str,
    key: &str,
    chat: &str,
    mid: &str,
    ts: i64,
    creator: &str,
    name: &str,
    content: &str,
    subject: Option<&str>,
) -> RawRecord {
    rec(
        source,
        key,
        json!({
            "id": mid, "conversationId": chat, "originalArrivalTime": ts as f64, "creator": creator,
            "imDisplayName": name, "messageType": "RichText/Html", "content": content,
            "properties": subject.map(|s| json!({"subject": s})).unwrap_or_else(|| json!({})),
        }),
    )
}

pub fn build_archive(home: &Path) -> Connection {
    std::env::set_var("TEAMS_CLI_HOME", home);
    let conn = archive::connect(Some(&home.join("archive.sqlite3"))).unwrap();
    let mentioned = json!([{"itemid": "0", "mri": ME, "displayName": "Me"}]).to_string();
    let mut group: Vec<Message> = (1..8)
        .filter(|i| *i != 4)
        .map(|i| {
            msg(
                GROUP,
                &format!("179500000000{i}"),
                ADA,
                &format!("<p>group note {i}</p>"),
                t((10 - i) as f64),
                "Ada Lovelace",
            )
        })
        .collect();
    group.push(msg(
        GROUP,
        EDITED,
        ADA,
        "<p>budget draft v1</p>",
        t(6.0),
        "Ada Lovelace",
    ));
    let mut review = msg(
        GROUP,
        "1795000000009",
        ADA,
        "<p>Me please review</p>",
        t(1.0),
        "Ada Lovelace",
    );
    review.properties = json!({"mentions": mentioned});
    group.push(review);
    group.push(msg(
        DM_BOB,
        "1795000000100",
        BOB,
        "<p>ping from bob</p>",
        t(2.0),
        "Bob Builder",
    ));
    archive::apply_snapshot(
        &conn,
        &snapshot(
            group,
            vec![conv(GROUP, "chat", "Launch crew"), conv(DM_BOB, "chat", "Bob Builder")],
            vec![
                mention_record("1795000000009", GROUP, ADA, "Ada Lovelace", t(1.0)),
                feed_item("a1", "mention", GROUP, "1795000000009", t(1.0), false),
                call_record(
                    "c1",
                    t(5.0),
                    Call {
                        chat: Some(GROUP),
                        ..Default::default()
                    },
                ),
                call_record(
                    "c2",
                    t(3.0),
                    Call {
                        state: "Missed",
                        direction: "Incoming",
                        dur: None,
                        originator: (BOB, Some("Bob B.")),
                        target: (ME, None),
                        ..Default::default()
                    },
                ),
                event(
                    "m1",
                    t(2.0) as f64,
                    t(1.0) as f64,
                    "Sprint review",
                    json!({"skypeTeamsDataObject": {"cid": MEETING_CHAT}}),
                ),
                saved_record("48:saved/1", GROUP, "1795000000001", t(4.0)),
                pinned_message(
                    PINNED_CHANNEL,
                    "p1",
                    GROUP,
                    "1795000000002",
                    t(8.0),
                    ADA,
                    "Ada Lovelace",
                    "<p>group note 2</p>",
                    Some("Rota"),
                ),
            ],
        ),
        None,
    )
    .unwrap();
    let mut edited = msg(GROUP, EDITED, ADA, "<p>final numbers</p>", t(6.0), "Ada Lovelace");
    edited.version = 2;
    edited.edited_at = Some(t(5.0));
    archive::apply_snapshot(&conn, &snapshot(vec![edited], vec![], vec![]), None).unwrap();
    conn.execute(
        "INSERT INTO sync_runs (started_at, finished_at, status) VALUES (1, 1, 'ok')",
        [],
    )
    .unwrap();
    conn
}

pub fn api_token() -> String {
    teams_cli::daemon::load_or_create_api_token().unwrap()
}

pub fn bearer() -> String {
    format!("Bearer {}", api_token())
}

pub fn home_with_archive() -> (tempfile::TempDir, PathBuf) {
    let td = tempfile::tempdir().unwrap();
    let home = td.path().to_path_buf();
    build_archive(&home);
    (td, home)
}
