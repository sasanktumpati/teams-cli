mod common;

use base64::Engine;
use common::*;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use teams_cli::daemon;
use teams_cli::live::compose::{self, BodyFormat, ComposeInput, Importance, MentionSpec, QuoteSpec};
use teams_cli::live::policy::{Principal, Scope, WriteRateLimiter};
use teams_cli::live::refs::{UploadedFile, UploadedImage};
use teams_cli::live::service::chats::{ChatStateRequest, CreateChatRequest, MemberChangeRequest, RenameChatRequest};
use teams_cli::live::service::messages::{
    EditMessageRequest, ForwardMessageRequest, MediaUpload, MessageActionRequest, ReactMessageRequest,
    SendMessageRequest,
};
use teams_cli::live::service::{ArchiveSource, LiveService};
use teams_cli::live::session::{SessionCache, SharePointSession, SkypeToken, TeamsBearerToken, TeamsSession};
use teams_cli::live::transport::ServiceEndpoints;

#[derive(Debug, Clone)]
struct RecordedReq {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: String,
}

fn mock_upstream<F>(
    expected_requests: usize,
    handler: F,
) -> (String, Arc<Mutex<Vec<RecordedReq>>>, thread::JoinHandle<()>)
where
    F: Fn(&RecordedReq) -> (u16, Vec<(&'static str, String)>, String) + Send + 'static,
{
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = server.server_addr().to_ip().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen_bg = Arc::clone(&seen);
    let h = thread::spawn(move || {
        for _ in 0..expected_requests {
            let Ok(mut req) = server.recv() else { break };
            let mut body = String::new();
            let _ = req.as_reader().read_to_string(&mut body);
            let rec = RecordedReq {
                method: req.method().as_str().to_string(),
                url: req.url().to_string(),
                headers: req
                    .headers()
                    .iter()
                    .map(|h| (h.field.as_str().to_string(), h.value.as_str().to_string()))
                    .collect(),
                body,
            };
            let (status, hdrs, resp_body) = handler(&rec);
            seen_bg.lock().unwrap().push(rec);
            let mut resp = tiny_http::Response::from_string(resp_body).with_status_code(status);
            for (k, v) in hdrs {
                resp = resp.with_header(tiny_http::Header::from_bytes(k, v).unwrap());
            }
            let _ = req.respond(resp);
        }
    });
    (format!("http://{addr}"), seen, h)
}

fn fake_session() -> Arc<SessionCache> {
    Arc::new(SessionCache::with_static_session(TeamsSession {
        skypetoken: Some(SkypeToken {
            token: "fake-skype-token-secret".to_string(),
            region: "amer".to_string(),
            expires_at: 4_000_000_000.0,
            skype_id: ME.to_string(),
            tenant_id: "tenant-1".to_string(),
            display_name: "Me User".to_string(),
        }),
        bearer: Some(TeamsBearerToken {
            token: "fake-bearer-token-secret".to_string(),
            audience: "https://api.spaces.skype.com".to_string(),
            expires_at: 4_000_000_000.0,
            tenant_id: "tenant-1".to_string(),
        }),
        sharepoint: Some(SharePointSession {
            personal_host: "contoso-my.sharepoint.com".to_string(),
            team_host: "contoso.sharepoint.com".to_string(),
            personal_cookie_header: Some("SPOIDCRL=fake-personal-sp-cookie".to_string()),
            team_cookie_header: Some("SPOIDCRL=fake-team-sp-cookie".to_string()),
        }),
        loaded_at: Instant::now(),
        expired_skypetoken_seen: false,
    }))
}

fn post_json_with(
    addr: SocketAddr,
    path: &str,
    bearer_tok: &str,
    extra_headers: &[(&str, &str)],
    body: &str,
) -> (u16, Value) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut req = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: 127.0.0.1\r\n\
         Authorization: Bearer {bearer_tok}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n",
        body.len()
    );
    for (k, v) in extra_headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    s.write_all(req.as_bytes()).unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    let (head, resp_body) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
    let code = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .unwrap_or(0);
    let val = serde_json::from_str(resp_body).unwrap_or(Value::Null);
    (code, val)
}

fn get_with(addr: SocketAddr, path: &str, bearer_tok: &str) -> (u16, Value) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let req = format!(
        "GET {path} HTTP/1.1\r\n\
         Host: 127.0.0.1\r\n\
         Authorization: Bearer {bearer_tok}\r\n\
         Connection: close\r\n\r\n"
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    let (head, resp_body) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
    let code = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .unwrap_or(0);
    let val = serde_json::from_str(resp_body).unwrap_or(Value::Null);
    (code, val)
}

#[test]
fn compose_markdown_escapes_html_and_builds_mentions_quotes_media() {
    let md_html = compose::render_markdown_to_teams_html(
        "**Bold** and `code` and <script>alert(1)</script> and [ok](https://example.com) and [bad](javascript:alert(1))",
    );
    assert!(md_html.contains("<strong>Bold</strong>"), "{md_html}");
    assert!(md_html.contains("<code>code</code>"), "{md_html}");
    assert!(md_html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"), "{md_html}");
    assert!(md_html.contains("<a href=\"https://example.com\">ok</a>"), "{md_html}");
    assert!(!md_html.contains("javascript:"), "{md_html}");

    let composed = compose::compose_message(&ComposeInput {
        text: "Hello @Ada <Boss>, check **this**".to_string(),
        format: BodyFormat::Markdown,
        display_name: "Me User".to_string(),
        subject: Some("Release Notes".to_string()),
        importance: Importance::Urgent,
        mentions: vec![MentionSpec::person(ADA.to_string(), "Ada <Boss>".to_string())],
        quote: Some(QuoteSpec {
            message_id: MID.to_string(),
            sender_mri: BOB.to_string(),
            sender_name: "Bob".to_string(),
            arrived_at_ms: Some(1_795_000_000_000),
            text: "Original <quoted> line".to_string(),
        }),
        forward: None,
        images: vec![UploadedImage {
            object_id: "0-wus-d1-abc".to_string(),
            imgo_url: "https://us-api.asm.skype.com/v1/objects/0-wus-d1-abc/views/imgo".to_string(),
            filename: "shot.png".to_string(),
            content_type: "image/png".to_string(),
            size_bytes: 100,
        }],
        files: vec![UploadedFile {
            item_id: "guid-123".to_string(),
            file_name: "plan.pdf".to_string(),
            file_type: "pdf".to_string(),
            base_url: "https://contoso-my.sharepoint.com/personal/me".to_string(),
            object_url: "https://contoso-my.sharepoint.com/personal/me/Documents/Microsoft Teams Chat Files/plan.pdf"
                .to_string(),
            site_url: "https://contoso-my.sharepoint.com/personal/me".to_string(),
            server_relative_url: "/personal/me/Documents/Microsoft Teams Chat Files/plan.pdf".to_string(),
            share_url: Some("https://contoso-my.sharepoint.com/:b:/g/share123".to_string()),
            share_id: Some("share123".to_string()),
            is_channel: false,
            size_bytes: 2048,
        }],
        client_message_id: None,
        idempotency_key: Some("deploy-42".to_string()),
    })
    .unwrap();

    assert!(composed.content_html.contains("http://schema.skype.com/Reply"));
    assert!(composed.content_html.contains("Original &lt;quoted&gt; line"));
    assert!(composed.content_html.contains("http://schema.skype.com/Mention"));
    assert!(composed.content_html.contains("Ada &lt;Boss&gt;"));
    assert!(composed.content_html.contains("http://schema.skype.com/AMSImage"));
    assert!(composed
        .content_html
        .contains("<attachment id=\"guid-123\"></attachment>"));

    let props = &composed.payload["properties"];
    assert_eq!(props["subject"], "Release Notes");
    assert_eq!(props["importance"], "urgent");
    assert!(props["mentions"].is_string(), "mentions must be stringified JSON");
    assert!(props["files"].is_string(), "files must be stringified JSON");
    let mentions_parsed: Value = serde_json::from_str(props["mentions"].as_str().unwrap()).unwrap();
    assert_eq!(mentions_parsed[0]["mri"], ADA);
    let files_parsed: Value = serde_json::from_str(props["files"].as_str().unwrap()).unwrap();
    assert_eq!(files_parsed[0]["fileName"], "plan.pdf");
    assert_eq!(files_parsed[0]["fileChicletState"]["serviceName"], "p2p");
    assert_eq!(props["ams_references"][0], "0-wus-d1-abc");

    let cid2 = compose::resolve_client_message_id(None, Some("deploy-42")).unwrap();
    assert_eq!(composed.client_message_id, cid2);

    assert_eq!(compose::normalize_reaction_key("👍").unwrap(), "like");
    assert_eq!(compose::normalize_reaction_key("❤️").unwrap(), "heart");
    assert_eq!(compose::normalize_reaction_key("🎉").unwrap(), "1f389_partypopper");
    assert_eq!(compose::normalize_reaction_key("custom_key").unwrap(), "custom_key");
    assert!(compose::normalize_reaction_key("???").is_err());
}

#[test]
fn live_service_uploads_image_and_file_and_executes_all_actions() {
    let _g = env_lock();
    let (_td, home) = home_with_archive();
    std::env::set_var("TEAMS_CLI_HOME", &home);
    std::env::set_var("TEAMS_CLI_ENABLE_WRITES", "1");
    std::env::remove_var("TEAMS_CLI_WRITE_ALLOWLIST");

    let (base, seen, h) = mock_upstream(19, |req| {
        if req.url == "/v1/objects" && req.method == "POST" {
            return (201, vec![], r#"{"id":"0-ams-img-1"}"#.to_string());
        }
        if req.url == "/v1/objects/0-ams-img-1/content/imgpsh" && req.method == "PUT" {
            return (201, vec![], String::new());
        }
        if req.url == "/_api/web/currentuser" {
            return (200, vec![], r#"{"Email":"me@contoso.com"}"#.to_string());
        }
        if req.url.ends_with("/_api/contextinfo") {
            return (200, vec![], r#"{"FormDigestValue":"digest-xyz"}"#.to_string());
        }
        if req.url.contains("/Files/add(") {
            return (
                200,
                vec![],
                r#"{"UniqueId":"sp-guid-99","Name":"spec.pdf","ServerRelativeUrl":"/personal/me_contoso_com/Documents/Microsoft Teams Chat Files/spec.pdf"}"#
                    .to_string(),
            );
        }
        if req.url.contains("/ShareLink") {
            return (
                200,
                vec![],
                r#"{"sharingLinkInfo":{"Url":"https://contoso-my.sharepoint.com/:b:/g/link99","ShareId":"sh-99"}}"#
                    .to_string(),
            );
        }
        if req.url == "/api/chatsvc/amer/v1/threads" && req.method == "POST" {
            return (
                201,
                vec![(
                    "Location",
                    "https://teams.microsoft.com/api/chatsvc/amer/v1/threads/19%3Anew%40thread.v2?foo=1".to_string(),
                )],
                "{}".to_string(),
            );
        }
        if req.url == "/v1/me/presence" && req.method == "GET" {
            return (
                200,
                vec![],
                r#"{"availability":"Available","activity":"Available"}"#.to_string(),
            );
        }
        if req.url.contains("/messages") && req.method == "POST" {
            return (201, vec![], r#"{"OriginalArrivalTime":1799999999000}"#.to_string());
        }
        (200, vec![], "{}".to_string())
    });

    let live = LiveService::with_session(
        fake_session(),
        ServiceEndpoints {
            chatsvc_base: base.clone(),
            ams_base: base.clone(),
            presence_base: base.clone(),
            sharepoint_base_override: Some(base),
        },
        true,
    );
    let conn = teams_cli::storage::archive::connect_readonly(Some(&home.join("archive.sqlite3"))).unwrap();
    let cli_p = Principal::cli();

    let sent = live
        .send_message(
            ArchiveSource::Conn(&conn),
            &cli_p,
            SendMessageRequest {
                target: "Launch crew".to_string(),
                text: "See **attached** @Ada".to_string(),
                format: BodyFormat::Markdown,
                reply_to: None,
                quote: Some(MID.to_string()),
                subject: Some("Weekly".to_string()),
                importance: Importance::High,
                mentions: vec!["ada@corp.test".to_string()],
                images: vec![MediaUpload {
                    filename: "shot.png".to_string(),
                    content_type: "image/png".to_string(),
                    bytes: vec![137, 80, 78, 71, 1, 2, 3],
                }],
                files: vec![MediaUpload {
                    filename: "spec.pdf".to_string(),
                    content_type: "application/pdf".to_string(),
                    bytes: b"%PDF-1.7 test".to_vec(),
                }],
                client_message_id: None,
                idempotency_key: Some("k-1".to_string()),
                dry_run: false,
            },
        )
        .unwrap();
    assert_eq!(sent["sent"], true);
    assert_eq!(sent["archiveVisible"], false);
    assert_eq!(sent["imagesUploaded"][0]["objectId"], "0-ams-img-1");
    assert_eq!(sent["filesUploaded"][0]["itemId"], "sp-guid-99");

    let react = live
        .react_message(
            ArchiveSource::Conn(&conn),
            &cli_p,
            ReactMessageRequest {
                chat: Some("Launch crew".to_string()),
                message_id: MID.to_string(),
                reaction: "👍".to_string(),
                add: true,
                dry_run: false,
            },
        )
        .unwrap();
    assert_eq!(react["reaction"], "like");

    let pin = live
        .pin_message(
            ArchiveSource::Conn(&conn),
            &cli_p,
            MessageActionRequest {
                chat: None,
                message_id: format!("{GROUP}/{MID}"),
                dry_run: false,
            },
            true,
        )
        .unwrap();
    assert_eq!(pin["pinned"], true);

    let save = live
        .save_message(
            ArchiveSource::Conn(&conn),
            &cli_p,
            MessageActionRequest {
                chat: None,
                message_id: format!("{GROUP}/{MID}"),
                dry_run: false,
            },
            true,
        )
        .unwrap();
    assert_eq!(save["saved"], true);

    let fwd = live
        .forward_message(
            ArchiveSource::Conn(&conn),
            &cli_p,
            ForwardMessageRequest {
                from_chat: None,
                message_id: format!("{GROUP}/{MID}"),
                to_chat: "48:notes".to_string(),
                comment: "FYI".to_string(),
                format: BodyFormat::Plain,
                idempotency_key: None,
                dry_run: false,
            },
        )
        .unwrap();
    assert_eq!(fwd["forwarded"], true);

    let err_other_author = live
        .edit_message(
            ArchiveSource::Conn(&conn),
            &cli_p,
            EditMessageRequest {
                chat: None,
                message_id: format!("{GROUP}/{MID}"),
                text: "hijack".to_string(),
                format: BodyFormat::Plain,
                dry_run: false,
            },
        )
        .unwrap_err();
    assert_eq!(err_other_author.code, "not_message_author");

    let created = live
        .create_chat(
            ArchiveSource::Conn(&conn),
            &cli_p,
            CreateChatRequest {
                members: vec!["ada@corp.test".to_string(), "bob@corp.test".to_string()],
                topic: Some("Sync".to_string()),
                dry_run: false,
            },
        )
        .unwrap();
    assert_eq!(created["conversationId"], "19:new@thread.v2");

    let _ = live
        .rename_chat(
            ArchiveSource::Conn(&conn),
            &cli_p,
            RenameChatRequest {
                chat: "Launch crew".to_string(),
                topic: "Launch crew v2".to_string(),
                dry_run: false,
            },
        )
        .unwrap();
    let _ = live
        .change_member(
            ArchiveSource::Conn(&conn),
            &cli_p,
            MemberChangeRequest {
                chat: "Launch crew".to_string(),
                member: "bob@corp.test".to_string(),
                add: true,
                dry_run: false,
            },
        )
        .unwrap();
    let _ = live
        .set_read_state(
            ArchiveSource::Conn(&conn),
            &cli_p,
            ChatStateRequest {
                chat: "Launch crew".to_string(),
                dry_run: false,
            },
            true,
        )
        .unwrap();
    let _ = live
        .set_mute_state(
            ArchiveSource::Conn(&conn),
            &cli_p,
            ChatStateRequest {
                chat: "Launch crew".to_string(),
                dry_run: false,
            },
            true,
        )
        .unwrap();

    let pres = live.get_presence(ArchiveSource::None, &cli_p, &[]).unwrap();
    assert_eq!(pres["availability"], "Available");
    let _ = live.set_presence(&cli_p, "dnd", Some("1h"), false).unwrap();
    let _ = live
        .set_status_message(&cli_p, Some("Heads down"), false, Some("2h"), false)
        .unwrap();

    h.join().unwrap();
    let recorded = seen.lock().unwrap();
    assert_eq!(recorded.len(), 19);
    let ams_create = recorded.iter().find(|r| r.url == "/v1/objects").unwrap();
    assert!(ams_create
        .headers
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("Authorization") && v.starts_with("skype_token ")));
    assert!(ams_create.body.contains("pish/image"));
    let sp_upload = recorded.iter().find(|r| r.url.contains("/Files/add(")).unwrap();
    assert!(sp_upload
        .headers
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("X-RequestDigest") && v == "digest-xyz"));
    let chat_send = recorded
        .iter()
        .find(|r| r.method == "POST" && r.url.contains("/conversations/") && r.url.ends_with("/messages"))
        .unwrap();
    assert!(chat_send.body.contains("0-ams-img-1"));
    assert!(chat_send.body.contains("sp-guid-99"));

    let audit = std::fs::read_to_string(home.join("write-audit.jsonl")).unwrap();
    assert!(audit.contains("message.send"));
    assert!(audit.contains("chat.create"));
    assert!(
        !audit.contains("See **attached**"),
        "audit log must never store plaintext bodies"
    );
    std::env::remove_var("TEAMS_CLI_ENABLE_WRITES");
}

#[test]
fn scoped_tokens_and_http_live_routes_enforce_access_control() {
    let _g = env_lock();
    let (_td, home) = home_with_archive();
    std::env::set_var("TEAMS_CLI_HOME", &home);
    std::env::remove_var("TEAMS_CLI_WRITE_ALLOWLIST");

    let (base, seen, h) = mock_upstream(2, |req| {
        if req.url == "/v1/me/presence" {
            return (
                200,
                vec![],
                r#"{"availability":"Busy","activity":"InACall"}"#.to_string(),
            );
        }
        (201, vec![], r#"{"OriginalArrivalTime":1799999999999}"#.to_string())
    });

    let live = Arc::new(LiveService::with_session(
        fake_session(),
        ServiceEndpoints {
            chatsvc_base: base.clone(),
            ams_base: base.clone(),
            presence_base: base,
            sharepoint_base_override: None,
        },
        true,
    ));
    let daemon_handle = daemon::start_with_live(0, None, false, live).unwrap();
    let addr = daemon_handle.addr();

    let read_tok = daemon::create_named_token("reader", None, &[Scope::Read], None).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    let restricted_send_tok = daemon::create_named_token(
        "bot-notes",
        None,
        &[Scope::Send, Scope::PresenceRead],
        Some(vec!["48:notes".to_string()]),
    )
    .unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();

    let (code, _) = get_with(addr, "/v1/status", &read_tok);
    assert_eq!(code, 200);

    let (code, live_st) = get_with(addr, "/v1/live/status", &read_tok);
    assert_eq!(code, 200);
    assert_eq!(live_st["writesEnabled"], true);

    let (code, err) = post_json_with(addr, "/v1/sync", &read_tok, &[], "{}");
    assert_eq!(code, 403);
    assert_eq!(err["error"]["code"], "scope_required");

    let (code, err) = post_json_with(
        addr,
        "/v1/live/messages/send",
        &read_tok,
        &[],
        r#"{"target":"48:notes","text":"hi"}"#,
    );
    assert_eq!(code, 403);
    assert_eq!(err["error"]["code"], "scope_required");
    assert_eq!(err["error"]["details"]["requiredScope"], "send");

    let (code, err) = post_json_with(
        addr,
        "/v1/live/messages/send",
        &restricted_send_tok,
        &[],
        r#"{"target":"48:notes","text":"hi","dryrun":true}"#,
    );
    assert_eq!(code, 400, "typo field must fail deny_unknown_fields: {err}");

    let (code, err) = post_json_with(
        addr,
        "/v1/live/messages/send",
        &restricted_send_tok,
        &[],
        r#"{"target":"48:notes","text":"<b>hi</b>","format":"html"}"#,
    );
    assert_eq!(code, 400, "raw html must be rejected over HTTP: {err}");

    let (code, err) = post_json_with(
        addr,
        "/v1/live/messages/send",
        &restricted_send_tok,
        &[],
        &json!({"target": GROUP, "text": "blocked"}).to_string(),
    );
    assert_eq!(code, 403, "{err}");
    assert_eq!(err["error"]["code"], "chat_not_allowed");

    let (code, sent) = post_json_with(
        addr,
        "/v1/live/messages/send",
        &restricted_send_tok,
        &[("Idempotency-Key", "http-idem-1")],
        r#"{"target":"48:notes","text":"**hello**","format":"markdown"}"#,
    );
    assert_eq!(code, 200, "{sent}");
    assert_eq!(sent["sent"], true);
    assert_eq!(sent["messageId"], "1799999999999");

    let (code, pres) = get_with(addr, "/v1/live/presence", &restricted_send_tok);
    assert_eq!(code, 200, "{pres}");
    assert_eq!(pres["availability"], "Busy");

    h.join().unwrap();
    assert_eq!(seen.lock().unwrap().len(), 2);
    daemon_handle.shutdown();
}

#[test]
fn daemon_without_allow_writes_blocks_live_write_routes() {
    let _g = env_lock();
    let (_td, home) = home_with_archive();
    std::env::set_var("TEAMS_CLI_HOME", &home);

    let daemon_handle = daemon::start(0, None, false).unwrap();
    let addr = daemon_handle.addr();
    let send_tok = daemon::create_named_token("sender", None, &[Scope::Send], None).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();

    let (code, err) = post_json_with(
        addr,
        "/v1/live/messages/send",
        &send_tok,
        &[],
        r#"{"target":"48:notes","text":"hi"}"#,
    );
    assert_eq!(code, 403, "{err}");
    assert_eq!(err["error"]["code"], "writes_disabled");
    daemon_handle.shutdown();
}

#[test]
fn write_rate_limiter_enforces_window() {
    let rl = WriteRateLimiter::new(2, Duration::from_secs(60));
    assert!(rl.check_and_record("tok1").is_ok());
    assert!(rl.check_and_record("tok1").is_ok());
    let err = rl.check_and_record("tok1").unwrap_err();
    assert_eq!(err.code, "rate_limited");
    assert_eq!(err.http_status(), 429);
    assert!(err.retry_after_secs.is_some());
    assert!(rl.check_and_record("tok2").is_ok());
}

#[test]
fn inline_base64_attachment_dry_run_works_over_http() {
    let _g = env_lock();
    let (_td, home) = home_with_archive();
    std::env::set_var("TEAMS_CLI_HOME", &home);

    let daemon_handle = daemon::start(0, None, false).unwrap();
    let addr = daemon_handle.addr();
    let send_tok = daemon::create_named_token("sender", None, &[Scope::Send], None).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    let b64 = base64::engine::general_purpose::STANDARD.encode(b"hello file");
    let body = json!({
        "target": "48:notes",
        "text": "check file",
        "files": [{"name": "notes.txt", "dataBase64": b64}],
        "dryRun": true
    })
    .to_string();
    let (code, res) = post_json_with(addr, "/v1/live/messages/send", &send_tok, &[], &body);
    assert_eq!(code, 200, "{res}");
    assert_eq!(res["dryRun"], true);
    assert_eq!(res["files"][0]["name"], "notes.txt");
    daemon_handle.shutdown();
}

#[test]
fn audit_security_policy_and_dry_run_exfiltration_regressions() {
    let _g = env_lock();
    let (_td, home) = home_with_archive();
    std::env::set_var("TEAMS_CLI_HOME", &home);
    std::env::remove_var("TEAMS_CLI_WRITE_ALLOWLIST");

    let daemon_handle = daemon::start(0, None, false).unwrap();
    let addr = daemon_handle.addr();

    let send_notes_only = daemon::create_named_token(
        "send-notes",
        None,
        &[Scope::Send, Scope::ChatManage],
        Some(vec!["48:notes".to_string()]),
    )
    .unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();

    let send_read_notes_only = daemon::create_named_token(
        "send-read-notes",
        None,
        &[Scope::Read, Scope::Send],
        Some(vec!["48:notes".to_string()]),
    )
    .unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();

    let (code, err) = post_json_with(
        addr,
        "/v1/live/messages/send",
        &send_notes_only,
        &[],
        &json!({"target": GROUP, "text": "probe", "dryRun": true}).to_string(),
    );
    assert_eq!(code, 403, "{err}");
    assert_eq!(err["error"]["code"], "chat_not_allowed");

    let (code, err) = post_json_with(
        addr,
        "/v1/live/messages/forward",
        &send_notes_only,
        &[],
        &json!({
            "messageId": format!("{GROUP}/{MID}"),
            "toChat": "48:notes",
            "dryRun": true
        })
        .to_string(),
    );
    assert_eq!(code, 403, "{err}");
    assert_eq!(err["error"]["code"], "scope_required");
    assert_eq!(err["error"]["details"]["requiredScope"], "read");

    let (code, err) = post_json_with(
        addr,
        "/v1/live/messages/forward",
        &send_read_notes_only,
        &[],
        &json!({
            "messageId": format!("{GROUP}/{MID}"),
            "toChat": "48:notes",
            "dryRun": true
        })
        .to_string(),
    );
    assert_eq!(code, 403, "{err}");
    assert_eq!(err["error"]["code"], "chat_not_allowed");

    let (code, err) = post_json_with(
        addr,
        "/v1/live/messages/send",
        &send_notes_only,
        &[],
        &json!({
            "target": "48:notes",
            "text": "exfil",
            "quote": format!("{GROUP}/{MID}"),
            "dryRun": true
        })
        .to_string(),
    );
    assert_eq!(code, 403, "{err}");
    assert_eq!(err["error"]["code"], "scope_required");
    assert_eq!(err["error"]["details"]["requiredScope"], "read");

    let (code, err) = post_json_with(
        addr,
        "/v1/live/messages/send",
        &send_read_notes_only,
        &[],
        &json!({
            "target": "48:notes",
            "text": "exfil",
            "quote": format!("{GROUP}/{MID}"),
            "dryRun": true
        })
        .to_string(),
    );
    assert_eq!(code, 403, "{err}");
    assert_eq!(err["error"]["code"], "chat_not_allowed");

    let send_read_all = daemon::create_named_token("send-read-all", None, &[Scope::Read, Scope::Send], None).unwrap()
        ["token"]
        .as_str()
        .unwrap()
        .to_string();

    let (code, err) = post_json_with(
        addr,
        "/v1/live/messages/send",
        &send_read_all,
        &[],
        &json!({
            "target": "48:notes",
            "text": "cross-chat",
            "quote": format!("{GROUP}/{MID}"),
            "dryRun": true
        })
        .to_string(),
    );
    assert_eq!(code, 400, "{err}");

    let (code, err) = post_json_with(
        addr,
        "/v1/live/messages/send",
        &send_read_all,
        &[],
        &json!({
            "target": GROUP,
            "text": "missing-quote",
            "quote": "9999999999999",
            "dryRun": true
        })
        .to_string(),
    );
    assert_eq!(code, 404, "missing quoted message must return 404: {err}");

    let (code, err) = post_json_with(
        addr,
        "/v1/live/messages/forward",
        &send_read_all,
        &[],
        &json!({
            "messageId": format!("{GROUP}/9999999999999"),
            "toChat": "48:notes",
            "dryRun": true
        })
        .to_string(),
    );
    assert_eq!(code, 404, "missing forwarded message must return 404: {err}");

    let (code, err) = post_json_with(
        addr,
        "/v1/live/messages/send",
        &send_notes_only,
        &[],
        r#"{"target":"a","text":"probe","dryRun":true}"#,
    );
    assert_eq!(code, 403, "{err}");
    assert_eq!(err["error"]["code"], "scope_required");

    let (code, err) = post_json_with(
        addr,
        "/v1/live/messages/send",
        &send_read_notes_only,
        &[],
        r#"{"target":"a","text":"probe","dryRun":true}"#,
    );
    assert_eq!(code, 404, "{err}");
    assert!(!err["error"]["message"].as_str().unwrap_or("").contains("Launch crew"));

    let react_read_notes_only = daemon::create_named_token(
        "react-read-notes",
        None,
        &[Scope::Read, Scope::React],
        Some(vec!["48:notes".to_string()]),
    )
    .unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    let (code, err) = post_json_with(
        addr,
        "/v1/live/messages/react",
        &react_read_notes_only,
        &[],
        &json!({"messageId": MID, "reaction": "like", "dryRun": true}).to_string(),
    );
    assert_eq!(
        code, 404,
        "bare messageId outside allowlist must not leak chat id: {err}"
    );
    assert!(!err["error"]["message"].as_str().unwrap_or("").contains(GROUP));

    let (code, err) = post_json_with(
        addr,
        "/v1/live/chats/create",
        &send_notes_only,
        &[],
        r#"{"members":["a"],"dryRun":true}"#,
    );
    assert_eq!(code, 403, "{err}");
    assert_eq!(err["error"]["code"], "chat_not_allowed");
    assert!(!err["error"]["message"].as_str().unwrap_or("").contains("ada@corp.test"));

    let small_b64 = base64::engine::general_purpose::STANDARD.encode(b"abc");
    let too_many_files: Vec<Value> = (0..11)
        .map(|i| json!({"name": format!("f{i}.txt"), "dataBase64": small_b64}))
        .collect();
    let (code, err) = post_json_with(
        addr,
        "/v1/live/messages/send",
        &send_notes_only,
        &[],
        &json!({
            "target": "48:notes",
            "text": "x",
            "files": too_many_files,
            "dryRun": true
        })
        .to_string(),
    );
    assert_eq!(code, 400, "more than 10 files must be rejected: {err}");

    assert!(daemon::create_named_token("empty-chats", None, &[Scope::Send], Some(vec![])).is_err());
    assert!(daemon::create_named_token("blank-chats", None, &[Scope::Send], Some(vec!["  ".to_string()])).is_err());

    {
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let req = format!(
            "POST /v1/live/messages/send HTTP/1.1\r\n\
             Host: 127.0.0.1\r\n\
             Authorization: Bearer {send_notes_only}\r\n\
             Content-Type: text/plain\r\n\
             Content-Length: 2\r\n\
             Connection: close\r\n\r\n{{}}"
        );
        s.write_all(req.as_bytes()).unwrap();
        let mut raw = String::new();
        s.read_to_string(&mut raw).unwrap();
        assert!(raw.starts_with("HTTP/1.1 415 "), "{raw}");
    }

    {
        let conn = teams_cli::storage::archive::connect(Some(&home.join("archive.sqlite3"))).unwrap();
        conn.execute(
            "INSERT INTO messages (conversation_id, message_id, kind, arrived_at, sender_mri, sender_name, text, first_seen_at, last_seen_at) \
             VALUES ('48:notes', ?1, 'message', 1795000000100, ?2, 'Me', 'dup id', 1, 1)",
            rusqlite::params![MID, ME],
        )
        .unwrap();
    }
    let (code, err) = get_with(addr, &format!("/v1/messages?around={MID}"), &send_read_all);
    assert_eq!(code, 400, "{err}");
    assert_eq!(err["error"]["code"], "ambiguous_message");
    assert!(
        err["error"]["details"]["chats"]
            .as_array()
            .is_some_and(|a| a.len() >= 2),
        "ApiError details must be preserved over HTTP: {err}"
    );
    let (code, err) = post_json_with(
        addr,
        "/v1/live/messages/send",
        &send_notes_only,
        &[],
        &json!({
            "target": "48:notes",
            "text": "x",
            "files": [{"name": "..", "dataBase64": small_b64}],
            "dryRun": true
        })
        .to_string(),
    );
    assert_eq!(code, 400, "{err}");

    let half_over_b64 = base64::engine::general_purpose::STANDARD.encode(vec![0x41u8; 4_300_000]);
    let (code, err) = post_json_with(
        addr,
        "/v1/live/messages/send",
        &send_notes_only,
        &[],
        &json!({
            "target": "48:notes",
            "text": "x",
            "images": [{"name": "a.png", "contentType": "image/png", "dataBase64": half_over_b64}],
            "files": [{"name": "b.bin", "dataBase64": half_over_b64}],
            "dryRun": true
        })
        .to_string(),
    );
    assert_eq!(code, 400, "combined images+files > 8 MiB must fail: {err}");

    let pres_tok = daemon::create_named_token("pres", None, &[Scope::PresenceRead, Scope::PresenceWrite], None)
        .unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    let too_many = (0..55).map(|i| format!("u{i}")).collect::<Vec<_>>().join(",");
    let (code, err) = get_with(addr, &format!("/v1/live/presence?person={too_many}"), &pres_tok);
    assert_eq!(code, 400, "{err}");

    let (code, err) = post_json_with(
        addr,
        "/v1/live/presence/message",
        &pres_tok,
        &[],
        r#"{"message":"   ","clear":false,"dryRun":true}"#,
    );
    assert_eq!(
        code, 400,
        "whitespace status message without clear:true must fail: {err}"
    );

    let (code, ok_never) = post_json_with(
        addr,
        "/v1/live/presence/set",
        &pres_tok,
        &[],
        r#"{"availability":"dnd","expires":"never","dryRun":true}"#,
    );
    assert_eq!(code, 200, "{ok_never}");
    assert!(ok_never["expiresAt"].is_null());

    daemon_handle.shutdown();
}

#[test]
fn compose_mentions_handle_overlapping_prefixes_and_href_urls() {
    let composed = compose::compose_message(&ComposeInput {
        text: "See [profile](https://example.com/@Alice) and `@Alice` and ping @Alice Smith and @Alice!".to_string(),
        format: BodyFormat::Markdown,
        display_name: "Me".to_string(),
        subject: None,
        importance: Importance::Normal,
        mentions: vec![
            MentionSpec::person("8:orgid:1".to_string(), "Alice".to_string()),
            MentionSpec::person("8:orgid:2".to_string(), "Alice Smith".to_string()),
        ],
        quote: None,
        forward: None,
        images: Vec::new(),
        files: Vec::new(),
        client_message_id: None,
        idempotency_key: None,
    })
    .unwrap();

    assert!(
        composed.content_html.contains("href=\"https://example.com/@Alice\""),
        "must not corrupt href attribute: {}",
        composed.content_html
    );
    assert!(
        composed.content_html.contains("<code>@Alice</code>"),
        "must not rewrite mentions inside code blocks: {}",
        composed.content_html
    );
    assert!(
        composed.content_html.contains("itemid=\"1\">Alice Smith</span>"),
        "longer mention must match first: {}",
        composed.content_html
    );
    assert!(
        composed.content_html.contains("itemid=\"0\">Alice</span>"),
        "shorter mention must match separately: {}",
        composed.content_html
    );

    let nested = compose::compose_message(&ComposeInput {
        text: "Hello @Team @Bob and @Bob".to_string(),
        format: BodyFormat::Plain,
        display_name: "Me".to_string(),
        subject: None,
        importance: Importance::Normal,
        mentions: vec![
            MentionSpec::person("8:orgid:1".to_string(), "Team @Bob".to_string()),
            MentionSpec::person("8:orgid:2".to_string(), "Bob".to_string()),
        ],
        quote: None,
        forward: None,
        images: Vec::new(),
        files: Vec::new(),
        client_message_id: None,
        idempotency_key: None,
    })
    .unwrap();
    assert!(
        nested.content_html.contains("itemid=\"0\">Team @Bob</span>"),
        "must not nest mention spans: {}",
        nested.content_html
    );
    assert!(
        nested
            .content_html
            .contains("and <span itemscope itemtype=\"http://schema.skype.com/Mention\" itemid=\"1\">Bob</span>"),
        "standalone @Bob outside span must still match: {}",
        nested.content_html
    );
}

#[test]
fn channel_file_upload_uses_team_cookie_and_fails_without_folder() {
    let _g = env_lock();
    let (_td, home) = home_with_archive();
    std::env::set_var("TEAMS_CLI_HOME", &home);
    std::env::set_var("TEAMS_CLI_ENABLE_WRITES", "1");
    std::env::remove_var("TEAMS_CLI_WRITE_ALLOWLIST");

    let chan_id = "19:chan1@thread.tacv2";
    {
        let conn = teams_cli::storage::archive::connect(Some(&home.join("archive.sqlite3"))).unwrap();
        conn.execute(
            "INSERT INTO conversations (id, kind, name, first_seen_at, last_seen_at) VALUES (?1, 'channel', 'Eng General', 1, 1)",
            [chan_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO record_versions (source, record_key, hash, data, captured_at) \
             VALUES ('conversation', ?1, 'h1', ?2, 1)",
            rusqlite::params![
                chan_id,
                r#"{"threadProperties":{"channelDocsFolderRelativeUrl":"/Sites/EngSite/Shared Documents/General"}}"#
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO conversations (id, kind, name, first_seen_at, last_seen_at) VALUES ('19:nofolder@thread.tacv2', 'channel', 'NoFolder', 1, 1)",
            [],
        )
        .unwrap();
    }

    let (base, seen, h) = mock_upstream(3, |req| {
        if req.url.ends_with("/_api/contextinfo") {
            return (200, vec![], r#"{"FormDigestValue":"chan-digest"}"#.to_string());
        }
        if req.url.contains("/Files/add(") {
            return (
                200,
                vec![],
                r#"{"UniqueId":"chan-guid-1","Name":"doc.txt","ServerRelativeUrl":"/sites/EngSite/Shared Documents/General/doc.txt"}"#
                    .to_string(),
            );
        }
        (201, vec![], r#"{"OriginalArrivalTime":1799999999111}"#.to_string())
    });

    let live = LiveService::with_session(
        fake_session(),
        ServiceEndpoints {
            chatsvc_base: base.clone(),
            ams_base: base.clone(),
            presence_base: base.clone(),
            sharepoint_base_override: Some(base),
        },
        true,
    );
    let conn = teams_cli::storage::archive::connect_readonly(Some(&home.join("archive.sqlite3"))).unwrap();
    let cli_p = Principal::cli();

    let err_dry = live
        .send_message(
            ArchiveSource::Conn(&conn),
            &cli_p,
            SendMessageRequest {
                target: "NoFolder".to_string(),
                text: "hi".to_string(),
                format: BodyFormat::Plain,
                reply_to: None,
                quote: None,
                subject: None,
                importance: Importance::Normal,
                mentions: Vec::new(),
                images: Vec::new(),
                files: vec![MediaUpload {
                    filename: "doc.txt".to_string(),
                    content_type: "text/plain".to_string(),
                    bytes: b"hello".to_vec(),
                }],
                client_message_id: None,
                idempotency_key: None,
                dry_run: true,
            },
        )
        .unwrap_err();
    assert_eq!(err_dry.code, "bad_param");

    let err = live
        .send_message(
            ArchiveSource::Conn(&conn),
            &cli_p,
            SendMessageRequest {
                target: "NoFolder".to_string(),
                text: "hi".to_string(),
                format: BodyFormat::Plain,
                reply_to: None,
                quote: None,
                subject: None,
                importance: Importance::Normal,
                mentions: Vec::new(),
                images: Vec::new(),
                files: vec![MediaUpload {
                    filename: "doc.txt".to_string(),
                    content_type: "text/plain".to_string(),
                    bytes: b"hello".to_vec(),
                }],
                client_message_id: None,
                idempotency_key: None,
                dry_run: false,
            },
        )
        .unwrap_err();
    assert_eq!(err.code, "bad_param");

    let ok = live
        .send_message(
            ArchiveSource::Conn(&conn),
            &cli_p,
            SendMessageRequest {
                target: "Eng General".to_string(),
                text: "channel file".to_string(),
                format: BodyFormat::Plain,
                reply_to: None,
                quote: None,
                subject: None,
                importance: Importance::Normal,
                mentions: Vec::new(),
                images: Vec::new(),
                files: vec![MediaUpload {
                    filename: "doc.txt".to_string(),
                    content_type: "text/plain".to_string(),
                    bytes: b"hello".to_vec(),
                }],
                client_message_id: None,
                idempotency_key: None,
                dry_run: false,
            },
        )
        .unwrap();
    assert_eq!(ok["sent"], true);

    h.join().unwrap();
    let recorded = seen.lock().unwrap();
    let sp_upload = recorded.iter().find(|r| r.url.contains("/Files/add(")).unwrap();
    assert!(
        sp_upload
            .headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("Cookie") && v.contains("fake-team-sp-cookie")),
        "channel upload must send team_cookie_header, got {:?}",
        sp_upload.headers
    );
    std::env::remove_var("TEAMS_CLI_ENABLE_WRITES");
}

#[test]
fn completions_support_msg_alias_and_variadic_positionals() {
    use clap::CommandFactory;
    let _g = env_lock();
    let (_td, home) = home_with_archive();
    std::env::set_var("TEAMS_CLI_HOME", &home);

    let root = teams_cli::cli::Cli::command();

    let msg_subs = teams_cli::completions::complete_argv(&root, &["msg".to_string(), String::new()], "fish");
    assert!(
        msg_subs.items.iter().any(|(v, _)| v == "send"),
        "expected `msg` alias subcommands, got {:?}",
        msg_subs.items
    );

    let msg_send_target = teams_cli::completions::complete_argv(
        &root,
        &["msg".to_string(), "send".to_string(), "Launch".to_string()],
        "fish",
    );
    assert!(
        msg_send_target.items.iter().any(|(v, _)| v.contains("Launch crew")),
        "expected chat completion under `msg send`, got {:?}",
        msg_send_target.items
    );

    let chat_new_second_member = teams_cli::completions::complete_argv(
        &root,
        &[
            "chat".to_string(),
            "new".to_string(),
            "ada@corp.test".to_string(),
            "Bo".to_string(),
        ],
        "fish",
    );
    assert!(
        chat_new_second_member.items.iter().any(|(v, _)| v.contains("Bob")),
        "expected variadic second person completion under `chat new`, got {:?}",
        chat_new_second_member.items
    );

    let presence_for = teams_cli::completions::complete_argv(
        &root,
        &[
            "presence".to_string(),
            "set".to_string(),
            "dnd".to_string(),
            "--for".to_string(),
            String::new(),
        ],
        "fish",
    );
    assert!(
        presence_for.items.iter().any(|(v, _)| v == "30m") && !presence_for.items.iter().any(|(v, _)| v == "90d"),
        "presence --for must suggest short durations: {:?}",
        presence_for.items
    );

    let comma_scope = teams_cli::completions::complete_argv(
        &root,
        &[
            "token".to_string(),
            "create".to_string(),
            "bot".to_string(),
            "--scope".to_string(),
            "read,s".to_string(),
        ],
        "bash",
    );
    assert!(
        comma_scope.items.iter().any(|(v, _)| v == "read,send")
            && comma_scope.items.iter().any(|(v, _)| v == "read,sync")
            && !comma_scope.items.iter().any(|(v, _)| v == "read,read"),
        "comma-delimited --scope completion must preserve prefix and skip chosen items: {:?}",
        comma_scope.items
    );
}

#[test]
fn views_mentions_tolerates_null_message_map_and_non_string_fields() {
    let _g = env_lock();
    let (_td, home) = home_with_archive();
    let conn = teams_cli::storage::archive::connect(Some(&home.join("archive.sqlite3"))).unwrap();
    let now = teams_cli::timeutil::now_ms();
    conn.execute(
        "INSERT INTO record_versions (source, record_key, hash, data, captured_at) \
         VALUES (?1, 'null-map', 'h-null', '{\"messageMap\":null}', 1)",
        [teams_cli::query::views::MENTIONS],
    )
    .unwrap();
    let mixed_json = json!({
        "messageMap": {
            "bad": null,
            "ok": {
                "content": 123,
                "messageType": 456,
                "properties": {
                    "activity": {
                        "sourceThreadId": GROUP,
                        "sourceMessageId": "99999",
                        "activityTimestamp": now,
                        "sourceUserImDisplayName": "Ada",
                        "messagePreview": "Ping @Me"
                    }
                }
            }
        }
    })
    .to_string();
    conn.execute(
        "INSERT INTO record_versions (source, record_key, hash, data, captured_at) \
         VALUES (?1, 'mixed-map', 'h-mix', ?2, 1)",
        rusqlite::params![teams_cli::query::views::MENTIONS, mixed_json],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO record_versions (source, record_key, hash, data, captured_at) \
         VALUES (?1, 'odd-feed', 'h-feed', '{\"activityType\":123,\"isRead\":\"yes\"}', 1)",
        [teams_cli::query::views::FEED],
    )
    .unwrap();

    let res = teams_cli::query::views::mentions(&conn, None, Some("all"), None, false, 50).unwrap();
    let list = res["mentions"].as_array().unwrap();
    assert!(
        list.iter().any(|m| m["messageId"] == "99999"),
        "tolerant mention chain deserialization must preserve valid entries: {res}"
    );
}

fn make_jwt(payload: Value) -> String {
    let hdr = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
    let body = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap());
    format!("{hdr}.{body}.sig")
}

#[test]
fn session_extraction_and_cache_reloads_missing_credentials() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use teams_cli::live::cookies::Cookie;
    use teams_cli::live::session::extract_session_from_cookies;

    let now = 1_800_000_000.0;
    let skype_jwt = make_jwt(json!({
        "rgn": "amer",
        "exp": now + 3600.0,
        "skypeid": "orgid:me-uuid",
        "tid": "t1"
    }));
    let other_aud_jwt = make_jwt(json!({
        "aud": "https://graph.microsoft.com",
        "exp": now + 9000.0,
        "tid": "t1"
    }));
    let teams_early_jwt = make_jwt(json!({
        "aud": "https://presence.teams.microsoft.com",
        "exp": now + 1000.0,
        "tid": "t1"
    }));
    let teams_late_jwt = make_jwt(json!({
        "aud": "https://api.spaces.skype.com",
        "exp": now + 5000.0,
        "tid": "t1"
    }));

    let cookies_personal_only = vec![
        Cookie {
            host_key: ".teams.microsoft.com".to_string(),
            name: "skypetoken_asm".to_string(),
            value: skype_jwt.clone(),
            path: "/".to_string(),
            expires_utc: 0,
            partitioned: false,
        },
        Cookie {
            host_key: ".teams.microsoft.com".to_string(),
            name: "authtoken".to_string(),
            value: format!("Bearer={other_aud_jwt}"),
            path: "/".to_string(),
            expires_utc: 0,
            partitioned: false,
        },
        Cookie {
            host_key: ".teams.microsoft.com".to_string(),
            name: "authtoken".to_string(),
            value: format!("Bearer={teams_early_jwt}"),
            path: "/".to_string(),
            expires_utc: 0,
            partitioned: false,
        },
        Cookie {
            host_key: ".teams.microsoft.com".to_string(),
            name: "authtoken".to_string(),
            value: format!("Bearer={teams_late_jwt}"),
            path: "/".to_string(),
            expires_utc: 0,
            partitioned: false,
        },
        Cookie {
            host_key: "contoso-my.sharepoint.com".to_string(),
            name: "SPOIDCRL".to_string(),
            value: "personal-auth".to_string(),
            path: "/".to_string(),
            expires_utc: 0,
            partitioned: false,
        },
        Cookie {
            host_key: ".sharepoint.com".to_string(),
            name: "MUID".to_string(),
            value: "telemetry-only".to_string(),
            path: "/".to_string(),
            expires_utc: 0,
            partitioned: false,
        },
    ];

    let extracted = extract_session_from_cookies(&cookies_personal_only, now);
    assert_eq!(extracted.bearer.as_ref().unwrap().token, teams_late_jwt);
    let sp = extracted.sharepoint.as_ref().unwrap();
    assert!(sp.personal_cookie_header.is_some());
    assert!(
        sp.team_cookie_header.is_none(),
        "telemetry .sharepoint.com cookie must not mark team SharePoint auth ready: {:?}",
        sp.team_cookie_header
    );

    let phase = Arc::new(AtomicUsize::new(0));
    let phase_bg = Arc::clone(&phase);
    let skype_for_loader = skype_jwt.clone();
    let bearer_for_loader = teams_late_jwt.clone();
    let cache = SessionCache::with_loader_and_retry(
        move |_now_ts| {
            let p = phase_bg.load(Ordering::SeqCst);
            let mut list = vec![Cookie {
                host_key: ".teams.microsoft.com".to_string(),
                name: "skypetoken_asm".to_string(),
                value: skype_for_loader.clone(),
                path: "/".to_string(),
                expires_utc: 0,
                partitioned: false,
            }];
            if p >= 1 {
                list.push(Cookie {
                    host_key: ".teams.microsoft.com".to_string(),
                    name: "authtoken".to_string(),
                    value: format!("Bearer={bearer_for_loader}"),
                    path: "/".to_string(),
                    expires_utc: 0,
                    partitioned: false,
                });
                list.push(Cookie {
                    host_key: "contoso.sharepoint.com".to_string(),
                    name: "FedAuth".to_string(),
                    value: "team-auth".to_string(),
                    path: "/".to_string(),
                    expires_utc: 0,
                    partitioned: false,
                });
            }
            Ok(list)
        },
        Duration::from_millis(20),
    );

    assert!(cache.skypetoken().is_ok());
    assert!(cache.bearer_token().is_err());
    assert!(cache.sharepoint_session_for(true).is_err());

    phase.store(1, Ordering::SeqCst);
    thread::sleep(Duration::from_millis(30));

    let bearer = cache.bearer_token().unwrap();
    assert_eq!(bearer.token, teams_late_jwt);
    let sp_team = cache.sharepoint_session_for(true).unwrap();
    assert!(sp_team.team_cookie_header.is_some());
    assert!(sp_team.personal_cookie_header.is_none());
}

#[test]
fn edit_and_delete_match_author_across_mri_variants() {
    let _g = env_lock();
    let (_td, home) = home_with_archive();
    let conn = teams_cli::storage::archive::connect(Some(&home.join("archive.sqlite3"))).unwrap();
    let alt_me = ME.replace("8:orgid:", "8:").to_ascii_uppercase();
    conn.execute(
        "INSERT INTO messages (conversation_id, message_id, kind, arrived_at, sender_mri, sender_name, text, first_seen_at, last_seen_at) \
         VALUES (?1, '1795000000999', 'message', 1795000000999, ?2, 'Me', 'own msg with variant mri', 1, 1)",
        rusqlite::params![GROUP, alt_me],
    )
    .unwrap();

    let live = LiveService::with_session(fake_session(), ServiceEndpoints::default(), true);
    let cli_p = Principal::cli();

    let edited = live
        .edit_message(
            ArchiveSource::Conn(&conn),
            &cli_p,
            EditMessageRequest {
                chat: Some(GROUP.to_string()),
                message_id: "1795000000999".to_string(),
                text: "updated".to_string(),
                format: BodyFormat::Plain,
                dry_run: true,
            },
        )
        .unwrap();
    assert_eq!(edited["dryRun"], true);

    let deleted = live
        .delete_message(
            ArchiveSource::Conn(&conn),
            &cli_p,
            MessageActionRequest {
                chat: Some(GROUP.to_string()),
                message_id: "1795000000999".to_string(),
                dry_run: true,
            },
        )
        .unwrap();
    assert_eq!(deleted["dryRun"], true);
}

#[test]
fn get_presence_validates_people_before_fetching_bearer_token() {
    let _g = env_lock();
    let (_td, home) = home_with_archive();
    let conn = teams_cli::storage::archive::connect_readonly(Some(&home.join("archive.sqlite3"))).unwrap();
    let empty_session = Arc::new(SessionCache::with_static_session(TeamsSession {
        skypetoken: None,
        bearer: None,
        sharepoint: None,
        loaded_at: Instant::now(),
        expired_skypetoken_seen: false,
    }));
    let live = LiveService::with_session(empty_session, ServiceEndpoints::default(), false);
    let cli_p = Principal::cli();

    let err = live
        .get_presence(ArchiveSource::Conn(&conn), &cli_p, &["no-such-person".to_string()])
        .unwrap_err();
    assert_eq!(
        err.code, "not_found",
        "invalid person must fail with 404 before bearer token lookup: {:?}",
        err
    );
}
