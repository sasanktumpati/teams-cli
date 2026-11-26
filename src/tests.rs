use crate::daemon;
use crate::identity;
use crate::ingest::extract::{Attachment, Conversation, Message, Person, RawRecord, Snapshot};
use crate::ingest::render;
use crate::live::compose::{BodyFormat, Importance};
use crate::live::policy::{self, Principal};
use crate::live::service::messages::SendMessageRequest;
use crate::live::service::{ArchiveSource, LiveService};
use crate::live::{cookies, fetch, local_onedrive, session};
use crate::query::{api, export, views};
use crate::storage::{archive, backups};
use crate::timeutil;
use base64::Engine;
use serde_json::json;
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::new(());

const CONV: &str = "19:abc@thread.v2";
const DM: &str = "19:aaaaaaaa-1111-1111-1111-111111111111_bbbbbbbb-2222-2222-2222-222222222222@unq.gbl.spaces";
const ME: &str = "8:orgid:aaaaaaaa-1111-1111-1111-111111111111";
const BOB: &str = "8:orgid:bbbbbbbb-2222-2222-2222-222222222222";
const ADA: &str = "8:orgid:cccccccc-3333-3333-3333-333333333333";

fn mk_msg(conv: &str, mid: &str, sender: &str, sname: &str, html: &str, ver: i64, arrived: i64) -> Message {
    let text = render::html_to_text(html, &HashMap::new());
    Message {
        conversation_id: conv.to_string(),
        message_id: mid.to_string(),
        thread_id: Some(mid.to_string()),
        parent_id: Some(mid.to_string()),
        version: ver,
        arrived_at: Some(arrived),
        edited_at: None,
        deleted_at: None,
        sender_mri: sender.to_string(),
        sender_name: sname.to_string(),
        message_type: "RichText/Html".to_string(),
        kind: "message".to_string(),
        content_html: html.to_string(),
        text,
        subject: String::new(),
        importance: String::new(),
        reactions: Vec::new(),
        attachments: Vec::new(),
        properties: json!({}),
        raw: json!({"id": mid}),
    }
}

fn mk_snap(messages: Vec<Message>, convs: Vec<Conversation>, records: Vec<RawRecord>) -> Snapshot {
    let mut people = HashMap::new();
    people.insert(
        ME.to_string(),
        Person {
            mri: ME.to_string(),
            name: "Me Myself".into(),
            email: "me@corp.test".into(),
            job_title: "Eng".into(),
            department: "Core".into(),
            kind: "person".into(),
            raw: json!({}),
        },
    );
    people.insert(
        BOB.to_string(),
        Person {
            mri: BOB.to_string(),
            name: "Bob Builder".into(),
            email: "bob@corp.test".into(),
            job_title: "PM".into(),
            department: "Product".into(),
            kind: "person".into(),
            raw: json!({}),
        },
    );
    people.insert(
        ADA.to_string(),
        Person {
            mri: ADA.to_string(),
            name: "Ada Lovelace".into(),
            email: "ada@corp.test".into(),
            job_title: "Staff".into(),
            department: "Eng".into(),
            kind: "person".into(),
            raw: json!({}),
        },
    );
    let conversations = convs.into_iter().map(|c| (c.id.clone(), c)).collect();
    Snapshot {
        me_mri: Some(ME.to_string()),
        people,
        conversations,
        messages,
        stats: BTreeMap::new(),
        records,
    }
}

#[test]
fn test_identity_and_timeutil() {
    assert!(identity::same_person(
        Some("8:orgid:AAAA-1111"),
        Some("8:orgid:aaaa-1111")
    ));
    assert_eq!(
        identity::one_on_one_oids("19:AAAA_BBBB@unq.gbl.spaces"),
        vec!["aaaa", "bbbb"]
    );
    assert_eq!(
        identity::clean_id(&json!(1795219003436.0)),
        Some("1795219003436".to_string())
    );
    assert_eq!(
        identity::clean_id(&json!("1795219003436.0")),
        Some("1795219003436".to_string())
    );
    assert!(timeutil::parse_when(Some("7d"), false).unwrap().is_some());
    assert!(timeutil::parse_when(Some("yesterday"), true).unwrap().is_some());
    assert!(timeutil::parse_when(Some("2026-11-01"), false).unwrap().is_some());
    assert!(timeutil::parse_when(Some("not-a-time"), false).is_err());
    assert!(timeutil::parse_iso_ms("2026-11-01T14:00:00.1234567Z").is_some());
}

#[test]
fn test_render_html_events_and_reactions() {
    let html = r#"<blockquote itemtype="http://schema.skype.com/Reply"><strong>Bob</strong><p>build green?</p></blockquote><p>yes</p>"#;
    assert_eq!(render::html_to_text(html, &HashMap::new()), "> Bob: build green?\nyes");

    let mut mentions = HashMap::new();
    mentions.insert("0".into(), ADA.into());
    mentions.insert("1".into(), ADA.into());
    let mhtml = r#"<p>Hi <span itemtype="http://schema.skype.com/Mention" itemid="0">Ada</span> <span itemtype="http://schema.skype.com/Mention" itemid="1">Lovelace</span>!</p>"#;
    assert_eq!(render::html_to_text(mhtml, &mentions), "Hi @Ada Lovelace!");

    assert_eq!(render::reaction_emoji("like"), "\u{1f44d}");
    assert_eq!(render::reaction_emoji("1f680_rocket"), "🚀");

    let ev = render::render_event(
        "ThreadActivity/AddMember",
        "<addmember><initiator>8:orgid:a</initiator><target>8:orgid:b</target></addmember>",
        &|m| if m.ends_with(":a") { "Ada".into() } else { "Bob".into() },
    );
    assert_eq!(ev.as_deref(), Some("Ada added Bob"));
}

#[test]
fn test_archive_durability_search_views_export_and_backups() {
    let _g = ENV_LOCK.lock().unwrap();
    let td = tempfile::tempdir().unwrap();
    std::env::set_var("TEAMS_CLI_HOME", td.path());

    let db_path = td.path().join("archive.sqlite3");
    let conn = archive::connect(Some(&db_path)).unwrap();
    assert_eq!(
        conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        archive::SCHEMA_VERSION
    );
    assert_eq!(
        conn.query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))
            .unwrap()
            .to_lowercase(),
        "wal"
    );

    let now = archive::now_ms();
    let mut m1 = mk_msg(
        CONV,
        "1001",
        ADA,
        "Ada Lovelace",
        "<p>budget draft v1</p>",
        1,
        now - 60_000,
    );
    m1.attachments.push(Attachment {
        kind: "file".into(),
        name: "Budget.xlsx".into(),
        url: "https://contoso.sharepoint.com/:x:/g/budget".into(),
    });
    let mut m2 = mk_msg(
        CONV,
        "1002",
        ADA,
        "Ada Lovelace",
        "<p>@Me please review</p>",
        1,
        now - 30_000,
    );
    m2.properties = json!({"mentions": serde_json::to_string(&json!([{"itemid": "0", "mri": ME}])).unwrap()});
    let m3 = mk_msg(DM, "1003", BOB, "Bob Builder", "<p>ping from bob</p>", 1, now - 10_000);

    let c_group = Conversation {
        id: CONV.into(),
        kind: "chat".into(),
        name: "Launch crew".into(),
        name_is_fallback: false,
        topic: String::new(),
        team_id: None,
        members: vec![ME.into(), ADA.into()],
        last_message_at: Some(now - 30_000),
        raw: json!({"id": CONV, "properties": {"consumptionhorizon": format!("{};1;1", now - 50_000)}, "threadProperties": {"isRead": false, "isLastMessageFromMe": false}}),
    };
    let c_dm = Conversation {
        id: DM.into(),
        kind: "chat".into(),
        name: "Bob Builder".into(),
        name_is_fallback: false,
        topic: String::new(),
        team_id: None,
        members: vec![],
        last_message_at: Some(now - 10_000),
        raw: json!({"id": DM}),
    };
    let recs = vec![
        RawRecord {
            source: "call-history-manager/call-history".into(),
            key: "c1".into(),
            data: json!({"callId": "c1", "startTime": now - 40_000, "callState": "Missed", "callDirection": "Incoming", "originatorParticipant": {"id": BOB, "displayName": "Bob"}}),
        },
        RawRecord {
            source: "calendar/calendar".into(),
            key: "m1".into(),
            data: json!({"objectId": "m1", "subject": "Sprint review", "startTime": now - 20_000, "endTime": now - 10_000, "organizerName": "Ada Lovelace", "isOnlineMeeting": true}),
        },
    ];
    let st = archive::apply_snapshot(
        &conn,
        &mk_snap(vec![m1, m2, m3], vec![c_group.clone(), c_dm.clone()], recs),
        None,
    )
    .unwrap();
    assert_eq!(st.get("messages_new").copied(), Some(3));

    let mut m1_v2 = mk_msg(
        CONV,
        "1001",
        ADA,
        "Ada Lovelace",
        "<p>final numbers</p>",
        2,
        now - 60_000,
    );
    m1_v2.edited_at = Some(now - 5_000);
    let st2 = archive::apply_snapshot(&conn, &mk_snap(vec![m1_v2], vec![], vec![]), None).unwrap();
    assert_eq!(st2.get("messages_edited").copied(), Some(1));

    let mut m3_del = mk_msg(DM, "1003", BOB, "Bob Builder", "", 2, now - 10_000);
    m3_del.deleted_at = Some(now - 2_000);
    let st3 = archive::apply_snapshot(&conn, &mk_snap(vec![m3_del], vec![], vec![]), None).unwrap();
    assert_eq!(st3.get("messages_deleted").copied(), Some(1));

    conn.execute(
        "INSERT INTO sync_runs (started_at, finished_at, status) VALUES (1, 1, 'ok')",
        [],
    )
    .unwrap();

    assert_eq!(
        api::search(
            &conn,
            Some("budget"),
            None,
            None,
            None,
            None,
            30,
            "recent",
            false,
            false,
            false
        )
        .unwrap()["matched"],
        0
    );
    let hs = api::search(
        &conn,
        Some("budget"),
        None,
        None,
        None,
        None,
        30,
        "recent",
        false,
        true,
        false,
    )
    .unwrap();
    assert_eq!(hs["matched"], 1);
    assert_eq!(hs["results"][0]["matchedInHistory"], true);
    let cs = api::search(
        &conn,
        Some("final"),
        None,
        None,
        None,
        None,
        30,
        "recent",
        false,
        false,
        true,
    )
    .unwrap();
    assert_eq!(cs["matched"], 1);
    assert_eq!(cs["countOnly"], true);

    let cm = api::list_chats(&conn, None, None, 50, false, Some("bob@corp.test")).unwrap();
    assert_eq!(cm["chats"].as_array().unwrap().len(), 1);
    assert_eq!(cm["chats"][0]["id"], DM);
    let pr = api::person(&conn, "Ada Lovelace", 20).unwrap();
    assert_eq!(pr["person"]["id"], ADA);

    let ar = api::read_messages(&conn, None, None, None, None, 200, false, None, false, Some("1001"), 5).unwrap();
    assert_eq!(ar["aroundMessageId"], "1001");
    assert_eq!(ar["messages"].as_array().unwrap().len(), 2);

    let inb = views::inbox(&conn, Some("7d"), 30).unwrap();
    assert_eq!(inb["counts"]["mentions"], 1);
    assert!(inb["counts"]["unreadChats"].as_i64().unwrap() >= 1);
    let mns = views::mentions(&conn, None, None, None, false, 50).unwrap();
    assert_eq!(mns["matched"], 1);
    let cl = views::calls(&conn, None, None, Some("bob"), true, 50).unwrap();
    assert_eq!(cl["matched"], 1);
    let mt = views::meetings(&conn, None, None, Some("sprint"), 50).unwrap();
    assert_eq!(mt["matched"], 1);

    let md = export::export_markdown(&conn, "Launch crew", None, None, None, false, true, None).unwrap();
    assert!(md.starts_with("# Launch crew"));
    assert!(md.contains("final numbers"));
    assert!(md.contains("budget draft v1"));

    let ro = archive::connect_readonly(Some(&db_path)).unwrap();
    assert!(ro
        .execute("INSERT INTO meta (key, value) VALUES ('x', 'y')", [])
        .is_err());
    drop(ro);

    let bk = backups::backup(&conn, &td.path().join("backups/snap.sqlite3.xz")).unwrap();
    let vr = backups::verify_archive(Some(&conn), None, true);
    assert_eq!(vr["ok"], true, "{vr}");
    let vt = backups::verify_archive(None, Some(&bk), false);
    assert_eq!(vt["ok"], true, "{vt}");
    drop(conn);

    let rr = backups::restore_backup(&bk, Some(&db_path), true).unwrap();
    assert_eq!(rr["messages"], 3);
}

#[test]
fn test_auth_cookies_onedrive_fetch_and_restricted_send() {
    let _g = ENV_LOCK.lock().unwrap();
    let td = tempfile::tempdir().unwrap();
    std::env::set_var("TEAMS_CLI_HOME", td.path());
    std::env::set_var("TEAMS_CLI_ONEDRIVE_DIR", td.path().join("od"));
    std::env::remove_var(policy::ENABLE_WRITES_ENV);
    std::env::remove_var(policy::ALLOWLIST_ENV);

    let pw = "test-safe-storage-secret";
    let key = cookies::derive_aes_key(pw);
    let enc = cookies::encrypt_cookie_value("secret-token-123", ".asm.skype.com", pw, 24);
    assert_eq!(
        cookies::decrypt_cookie_value(&enc, ".asm.skype.com", &key, 24, "").unwrap(),
        "secret-token-123"
    );
    assert!(cookies::decrypt_cookie_value(&enc, ".sharepoint.com", &key, 24, "").is_err());

    let cks = vec![
        cookies::Cookie {
            host_key: ".asm.skype.com".into(),
            name: "skypetoken_asm".into(),
            value: "TOK".into(),
            path: "/".into(),
            expires_utc: 0,
            partitioned: false,
        },
        cookies::Cookie {
            host_key: ".evil.com".into(),
            name: "x".into(),
            value: "BAD".into(),
            path: "/".into(),
            expires_utc: 0,
            partitioned: false,
        },
    ];
    assert_eq!(
        cookies::build_cookie_header(&cks, "https://us-api.asm.skype.com/v1/x", None),
        "skypetoken_asm=TOK"
    );
    assert_eq!(
        cookies::build_cookie_header(&cks, "http://us-api.asm.skype.com/v1/x", None),
        ""
    );
    assert_eq!(cookies::build_cookie_header(&cks, "https://evil.com/steal", None), "");

    assert_eq!(
        fetch::rewrite_download_url("https://contoso.sharepoint.com/:x:/g/personal/doc"),
        "https://contoso.sharepoint.com/:x:/g/personal/doc?download=1"
    );

    let od = td.path().join("od");
    std::fs::create_dir_all(&od).unwrap();
    std::fs::write(od.join("Plan.pdf"), b"pdf").unwrap();
    assert!(local_onedrive::find_onedrive_file("Plan.pdf", None).is_some());
    assert!(local_onedrive::find_onedrive_file("../Plan.pdf", None).is_none());
    assert!(local_onedrive::find_onedrive_file("/etc/passwd", None).is_none());

    let hdr = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"alg":"RS256"}"#);
    let pay = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(r#"{"rgn":"amer","exp":2000000000,"skypeid":"u1","tid":"t1"}"#);
    let jwt = format!("{hdr}.{pay}.sig");
    let tok = session::load_skypetoken(
        Some(&[cookies::Cookie {
            host_key: ".asm.skype.com".into(),
            name: "skypetoken_asm".into(),
            value: jwt,
            path: "/".into(),
            expires_utc: 0,
            partitioned: false,
        }]),
        Some(1_800_000_000.0),
    )
    .unwrap();
    assert_eq!(tok.region, "amer");

    let live = LiveService::default();
    let cli_p = Principal::cli();
    let dry = live
        .send_message(
            ArchiveSource::None,
            &cli_p,
            SendMessageRequest {
                target: "48:notes".into(),
                text: "Hello <world> & friends".into(),
                format: BodyFormat::Plain,
                reply_to: None,
                quote: None,
                subject: None,
                importance: Importance::Normal,
                mentions: Vec::new(),
                images: Vec::new(),
                files: Vec::new(),
                client_message_id: Some("12345".into()),
                idempotency_key: None,
                dry_run: true,
            },
        )
        .unwrap();
    assert_eq!(dry["dryRun"], true);
    assert_eq!(dry["restricted"], true);
    assert_eq!(dry["content"], "<p>Hello &lt;world&gt; &amp; friends</p>");
    let err = live
        .send_message(
            ArchiveSource::None,
            &cli_p,
            SendMessageRequest {
                target: "48:notes".into(),
                text: "hi".into(),
                format: BodyFormat::Plain,
                reply_to: None,
                quote: None,
                subject: None,
                importance: Importance::Normal,
                mentions: Vec::new(),
                images: Vec::new(),
                files: Vec::new(),
                client_message_id: None,
                idempotency_key: None,
                dry_run: false,
            },
        )
        .unwrap_err();
    assert!(err.message.contains(policy::ENABLE_WRITES_ENV));
}

#[test]
fn test_daemon_host_extraction_and_pool() {
    assert_eq!(daemon::extract_host(Some("[::1]:47613")), "::1");
    assert_eq!(daemon::extract_host(Some("[::1]")), "::1");
    assert_eq!(daemon::extract_host(Some("[::1].evil.com:80")), "");
    assert_eq!(daemon::extract_host(Some("[::1]:notaport")), "");
    assert_eq!(daemon::extract_host(Some("LOCALHOST:1")), "localhost");

    let mut q = HashMap::new();
    assert_eq!(daemon::since_param(&q, Some("7d")), Some("7d"));
    q.insert("since".into(), " all ".into());
    assert_eq!(daemon::since_param(&q, Some("7d")), None);
}
