use crate::live::error::LiveError;
use crate::live::refs::{UploadedFile, UploadedImage};
use crate::timeutil;
use jiff::Timestamp;
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use regex::Regex;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::sync::LazyLock;

static CLIENT_MSG_ID_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9]{1,32}$").unwrap());

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyFormat {
    Plain,
    Markdown,
    Html,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Importance {
    Normal,
    High,
    Urgent,
}

impl Importance {
    pub fn as_teams_str(&self) -> Option<&'static str> {
        match self {
            Importance::Normal => None,
            Importance::High => Some("high"),
            Importance::Urgent => Some("urgent"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MentionSpec {
    pub mri: String,
    pub display_name: String,
    pub mention_type: String,
}

impl MentionSpec {
    pub fn person(mri: String, display_name: String) -> Self {
        Self {
            mri,
            display_name,
            mention_type: "person".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct QuoteSpec {
    pub message_id: String,
    pub sender_mri: String,
    pub sender_name: String,
    pub arrived_at_ms: Option<i64>,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct ForwardSpec {
    pub message_id: String,
    pub conversation_id: String,
    pub chat_kind: String,
    pub sender_mri: String,
    pub sender_name: String,
    pub arrived_at_ms: Option<i64>,
    pub content_html: String,
    pub text: String,
    pub message_type: String,
}

#[derive(Debug, Clone)]
pub struct ComposeInput {
    pub text: String,
    pub format: BodyFormat,
    pub display_name: String,
    pub subject: Option<String>,
    pub importance: Importance,
    pub mentions: Vec<MentionSpec>,
    pub quote: Option<QuoteSpec>,
    pub forward: Option<ForwardSpec>,
    pub images: Vec<UploadedImage>,
    pub files: Vec<UploadedFile>,
    pub client_message_id: Option<String>,
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ComposedPayload {
    pub payload: Value,
    pub content_html: String,
    pub client_message_id: String,
}

pub fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

pub fn html_escape_text(s: &str) -> String {
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

pub fn format_plain_html(text: &str) -> String {
    let norm = text.replace("\r\n", "\n").replace('\r', "\n");
    let escaped = html_escape_text(&norm);
    escaped
        .split("\n\n")
        .map(|p| format!("<p>{}</p>", p.replace('\n', "<br>")))
        .collect::<Vec<_>>()
        .join("")
}

fn is_safe_href(href: &str) -> bool {
    let lower = href.trim().to_ascii_lowercase();
    lower.starts_with("https://") || lower.starts_with("http://") || lower.starts_with("mailto:")
}

pub fn render_markdown_to_teams_html(md: &str) -> String {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    let parser = Parser::new_ext(md, opts);
    let mut out = String::with_capacity(md.len() + 32);
    let mut open_link = false;

    for ev in parser {
        match ev {
            Event::Start(tag) => match tag {
                Tag::Paragraph => out.push_str("<p>"),
                Tag::Heading { level, .. } => {
                    let h = match level {
                        HeadingLevel::H1 => "h1",
                        HeadingLevel::H2 => "h2",
                        _ => "h3",
                    };
                    out.push('<');
                    out.push_str(h);
                    out.push('>');
                }
                Tag::BlockQuote(_) => out.push_str("<blockquote>"),
                Tag::CodeBlock(_) => out.push_str("<pre><code>"),
                Tag::List(None) => out.push_str("<ul>"),
                Tag::List(Some(_)) => out.push_str("<ol>"),
                Tag::Item => out.push_str("<li>"),
                Tag::Emphasis => out.push_str("<em>"),
                Tag::Strong => out.push_str("<strong>"),
                Tag::Strikethrough => out.push_str("<s>"),
                Tag::Link { dest_url, .. } => {
                    if is_safe_href(&dest_url) {
                        out.push_str(&format!("<a href=\"{}\">", html_escape(&dest_url)));
                        open_link = true;
                    } else {
                        open_link = false;
                    }
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => out.push_str("</p>"),
                TagEnd::Heading(level) => {
                    let h = match level {
                        HeadingLevel::H1 => "h1",
                        HeadingLevel::H2 => "h2",
                        _ => "h3",
                    };
                    out.push_str("</");
                    out.push_str(h);
                    out.push('>');
                }
                TagEnd::BlockQuote(_) => out.push_str("</blockquote>"),
                TagEnd::CodeBlock => out.push_str("</code></pre>"),
                TagEnd::List(false) => out.push_str("</ul>"),
                TagEnd::List(true) => out.push_str("</ol>"),
                TagEnd::Item => out.push_str("</li>"),
                TagEnd::Emphasis => out.push_str("</em>"),
                TagEnd::Strong => out.push_str("</strong>"),
                TagEnd::Strikethrough => out.push_str("</s>"),
                TagEnd::Link if open_link => {
                    out.push_str("</a>");
                    open_link = false;
                }
                _ => {}
            },
            Event::Text(t) => out.push_str(&html_escape_text(&t)),
            Event::Code(c) => {
                out.push_str("<code>");
                out.push_str(&html_escape_text(&c));
                out.push_str("</code>");
            }
            Event::Html(h) | Event::InlineHtml(h) => out.push_str(&html_escape_text(&h)),
            Event::SoftBreak | Event::HardBreak => out.push_str("<br>"),
            Event::Rule => out.push_str("<hr>"),
            _ => {}
        }
    }
    out
}

pub fn random_client_message_id() -> Result<String, LiveError> {
    let mut bytes = [0u8; 8];
    getrandom::getrandom(&mut bytes)
        .map_err(|e| LiveError::internal(format!("Could not generate a client message id: {e}")))?;
    let r = u64::from_le_bytes(bytes) % 9_000_000_000_000_000_000u64;
    Ok((1_000_000_000_000_000_000u64 + r).to_string())
}

pub fn idempotency_to_client_message_id(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    let mut arr = [0u8; 8];
    arr.copy_from_slice(&digest[..8]);
    let r = u64::from_le_bytes(arr) % 9_000_000_000_000_000_000u64;
    (1_000_000_000_000_000_000u64 + r).to_string()
}

pub fn resolve_client_message_id(explicit: Option<&str>, idempotency_key: Option<&str>) -> Result<String, LiveError> {
    if let Some(c) = explicit.filter(|s| !s.is_empty()) {
        if !CLIENT_MSG_ID_RE.is_match(c) {
            return Err(LiveError::bad("client_message_id must be 1 to 32 ASCII digits."));
        }
        return Ok(c.to_string());
    }
    if let Some(k) = idempotency_key.filter(|s| !s.trim().is_empty()) {
        return Ok(idempotency_to_client_message_id(k.trim()));
    }
    random_client_message_id()
}

fn render_quote_html(q: &QuoteSpec) -> String {
    let preview: String = q.text.chars().take(400).collect();
    let ts_attr = q
        .arrived_at_ms
        .map(|ms| format!("<span itemprop=\"time\" itemid=\"{ms}\"></span>"))
        .unwrap_or_default();
    format!(
        "<blockquote itemtype=\"http://schema.skype.com/Reply\" itemid=\"{}\">\
         <strong itemprop=\"mri\" itemid=\"{}\">{}</strong>{}\
         <p itemprop=\"preview\">{}</p></blockquote>",
        html_escape(&q.message_id),
        html_escape(&q.sender_mri),
        html_escape_text(&q.sender_name),
        ts_attr,
        html_escape_text(&preview)
    )
}

fn render_forward_html(f: &ForwardSpec) -> String {
    let inner = if !f.content_html.trim().is_empty() {
        f.content_html.clone()
    } else {
        format_plain_html(&f.text)
    };
    format!("<blockquote itemtype=\"http://schema.skype.com/Forward\">{inner}</blockquote>")
}

fn render_mention_span(item_id: usize, display_name: &str) -> String {
    format!(
        "<span itemscope itemtype=\"http://schema.skype.com/Mention\" itemid=\"{item_id}\">{}</span>",
        html_escape_text(display_name)
    )
}

fn build_file_chiclet(f: &UploadedFile) -> Value {
    let service_name = if f.is_channel { "teams" } else { "p2p" };
    let state = if f.is_channel { "active" } else { "reference" };
    json!({
        "@type": "http://schema.skype.com/File",
        "version": 2,
        "id": f.item_id,
        "itemid": f.item_id,
        "fileName": f.file_name,
        "fileType": f.file_type,
        "title": f.file_name,
        "type": f.file_type,
        "baseUrl": f.base_url,
        "objectUrl": f.object_url,
        "state": state,
        "fileInfo": {
            "itemId": f.item_id,
            "fileUrl": f.object_url,
            "siteUrl": f.site_url,
            "serverRelativeUrl": f.server_relative_url,
            "shareUrl": f.share_url,
            "shareId": f.share_id,
        },
        "fileChicletState": {
            "serviceName": service_name,
            "state": state,
        },
        "botFileProperties": {},
    })
}

fn update_skip_depth(tag_inner: &str, code_depth: &mut usize, pre_depth: &mut usize, span_depth: &mut usize) {
    let t = tag_inner.trim();
    if t.is_empty() || t.starts_with('!') || t.starts_with('?') {
        return;
    }
    let closing = t.starts_with('/');
    if !closing && t.ends_with('/') {
        return;
    }
    let rest = if closing { &t[1..] } else { t }.trim_start();
    let name = rest
        .split(|c: char| c.is_ascii_whitespace() || c == '/')
        .next()
        .unwrap_or("");
    let counter = if name.eq_ignore_ascii_case("code") {
        code_depth
    } else if name.eq_ignore_ascii_case("pre") {
        pre_depth
    } else if name.eq_ignore_ascii_case("span") {
        span_depth
    } else {
        return;
    };
    if closing {
        *counter = counter.saturating_sub(1);
    } else {
        *counter += 1;
    }
}

fn replace_mention_in_html_text(html: &str, needle: &str, replacement: &str) -> Option<String> {
    if needle.is_empty() {
        return None;
    }
    let bytes = html.as_bytes();
    let mut code_depth = 0usize;
    let mut pre_depth = 0usize;
    let mut span_depth = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            let tag_start = i + 1;
            let mut j = tag_start;
            while j < bytes.len() && bytes[j] != b'>' {
                j += 1;
            }
            if j >= bytes.len() {
                break;
            }
            update_skip_depth(&html[tag_start..j], &mut code_depth, &mut pre_depth, &mut span_depth);
            i = j + 1;
            continue;
        }
        let seg_start = i;
        while i < bytes.len() && bytes[i] != b'<' {
            i += 1;
        }
        if code_depth > 0 || pre_depth > 0 || span_depth > 0 {
            continue;
        }
        let seg = &html[seg_start..i];
        let mut search_from = 0usize;
        while let Some(rel) = seg[search_from..].find(needle) {
            let match_start = search_from + rel;
            let match_end = match_start + needle.len();
            let prev_ok = seg[..match_start]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_alphanumeric() && c != '_');
            let next_ok = seg[match_end..]
                .chars()
                .next()
                .is_none_or(|c| !c.is_alphanumeric() && c != '_');
            if prev_ok && next_ok {
                let abs_start = seg_start + match_start;
                let abs_end = seg_start + match_end;
                let mut out = String::with_capacity(html.len() + replacement.len());
                out.push_str(&html[..abs_start]);
                out.push_str(replacement);
                out.push_str(&html[abs_end..]);
                return Some(out);
            }
            search_from = match_start + 1;
            while search_from < seg.len() && !seg.is_char_boundary(search_from) {
                search_from += 1;
            }
        }
    }
    None
}

pub fn compose_message(input: &ComposeInput) -> Result<ComposedPayload, LiveError> {
    let has_text = !input.text.trim().is_empty();
    let has_media = !input.images.is_empty() || !input.files.is_empty();
    let has_forward = input.forward.is_some();
    if !has_text && !has_media && !has_forward {
        return Err(LiveError::bad(
            "Message text, attachment (--image / --file), or forwarded message is required.",
        ));
    }

    let cid = resolve_client_message_id(input.client_message_id.as_deref(), input.idempotency_key.as_deref())?;

    let mut content = String::new();

    if let Some(ref q) = input.quote {
        content.push_str(&render_quote_html(q));
    }

    if !input.mentions.is_empty() && !has_text {
        let spans: Vec<String> = input
            .mentions
            .iter()
            .enumerate()
            .map(|(i, m)| render_mention_span(i, &m.display_name))
            .collect();
        content.push_str(&format!("<p>{}</p>", spans.join(" ")));
    } else if has_text {
        let mut body_html = match input.format {
            BodyFormat::Plain => format_plain_html(&input.text),
            BodyFormat::Markdown => render_markdown_to_teams_html(&input.text),
            BodyFormat::Html => input.text.clone(),
        };
        if !input.mentions.is_empty() {
            let mut order: Vec<(usize, &MentionSpec)> = input.mentions.iter().enumerate().collect();
            order.sort_by(|(ia, a), (ib, b)| b.display_name.len().cmp(&a.display_name.len()).then(ia.cmp(ib)));
            let mut unmatched: Vec<(usize, String)> = Vec::new();
            for (i, m) in order {
                let needle = format!("@{}", html_escape_text(&m.display_name));
                let span = render_mention_span(i, &m.display_name);
                if let Some(updated) = replace_mention_in_html_text(&body_html, &needle, &span) {
                    body_html = updated;
                } else {
                    unmatched.push((i, span));
                }
            }
            if !unmatched.is_empty() {
                unmatched.sort_by_key(|(i, _)| *i);
                let prefix_spans: Vec<String> = unmatched.into_iter().map(|(_, s)| s).collect();
                content.push_str(&format!("<p>{}</p>", prefix_spans.join(" ")));
            }
        }
        content.push_str(&body_html);
    }

    if let Some(ref f) = input.forward {
        content.push_str(&render_forward_html(f));
    }

    let mut ams_refs = Vec::new();
    for img in &input.images {
        let ext = img
            .content_type
            .split('/')
            .nth(1)
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "png".to_string());
        let esc_id = html_escape(&img.object_id);
        content.push_str(&format!(
            "<p><img src=\"{}\" itemtype=\"http://schema.skype.com/AMSImage\" itemscope=\"{}\" alt=\"{}\" id=\"x_{}\" itemid=\"{}\" /></p>",
            html_escape(&img.imgo_url),
            html_escape(&ext),
            html_escape(&img.filename),
            esc_id,
            esc_id
        ));
        ams_refs.push(Value::String(img.object_id.clone()));
    }

    for f in &input.files {
        content.push_str(&format!("<attachment id=\"{}\"></attachment>", html_escape(&f.item_id)));
    }

    let mut props = Map::new();
    props.insert("formatVariant".to_string(), Value::String("TEAMS".to_string()));

    if let Some(ref subj) = input.subject {
        let s = subj.trim();
        if !s.is_empty() {
            props.insert("subject".to_string(), Value::String(s.to_string()));
        }
    }

    if let Some(imp) = input.importance.as_teams_str() {
        props.insert("importance".to_string(), Value::String(imp.to_string()));
    }

    if !input.mentions.is_empty() {
        let arr: Vec<Value> = input
            .mentions
            .iter()
            .enumerate()
            .map(|(i, m)| {
                json!({
                    "@type": "http://schema.skype.com/Mention",
                    "itemid": i,
                    "mri": m.mri,
                    "mentionType": m.mention_type,
                    "displayName": m.display_name,
                })
            })
            .collect();
        props.insert(
            "mentions".to_string(),
            Value::String(crate::json_codec::to_string(&arr)),
        );
    }

    if !ams_refs.is_empty() {
        props.insert("ams_references".to_string(), Value::Array(ams_refs));
    }

    if !input.files.is_empty() {
        let chiclets: Vec<Value> = input.files.iter().map(build_file_chiclet).collect();
        props.insert(
            "files".to_string(),
            Value::String(crate::json_codec::to_string(&chiclets)),
        );
    }

    if let Some(ref f) = input.forward {
        props.insert(
            "forwardTemplateId".to_string(),
            Value::String("basic_forward_message_template".to_string()),
        );
        let sent_iso = f
            .arrived_at_ms
            .and_then(|ms| timeutil::iso(Some(ms)))
            .unwrap_or_else(|| Timestamp::now().strftime("%Y-%m-%dT%H:%M:%S%.3fZ").to_string());
        let msg_num: Value = f
            .message_id
            .parse::<i64>()
            .map(Value::from)
            .unwrap_or_else(|_| Value::String(f.message_id.clone()));
        props.insert(
            "originalMessageContext".to_string(),
            json!({
                "originalSender": f.sender_mri,
                "originalSentTime": sent_iso,
                "messageId": msg_num,
                "originalThreadId": f.conversation_id,
                "threadType": f.chat_kind,
                "messageType": f.message_type,
            }),
        );
    }

    let composetime = Timestamp::now().strftime("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
    let payload = json!({
        "content": content,
        "messagetype": "RichText/Html",
        "contenttype": "text",
        "clientmessageid": cid,
        "imdisplayname": input.display_name,
        "composetime": composetime,
        "properties": Value::Object(props),
    });

    Ok(ComposedPayload {
        content_html: content,
        client_message_id: cid,
        payload,
    })
}

pub fn normalize_reaction_key(raw: &str) -> Result<String, LiveError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(LiveError::bad("Reaction emoji or key is required."));
    }
    let mapped = match trimmed.to_ascii_lowercase().as_str() {
        "👍" | "+1" | "thumbsup" | "thumbs_up" | "like" => "like",
        "❤️" | "❤" | "love" | "heart" => "heart",
        "😂" | "😄" | "haha" | "laugh" => "laugh",
        "😮" | "😲" | "wow" | "surprised" => "surprised",
        "😢" | "😭" | "cry" | "sad" => "sad",
        "😡" | "😠" | "mad" | "angry" => "angry",
        "🎉" | "tada" | "party" | "partypopper" => "1f389_partypopper",
        "👏" | "clap" | "clapping" | "clappinghands" => "clappinghands",
        "🙌" | "handsinair" => "handsinair",
        "🙏" | "pray" | "praying" | "thanks" => "praying",
        "💯" | "100" => "1f4af_hundredpointssymbol",
        "🔥" | "fire" => "1f525_fire",
        "👀" | "eyes" => "1f440_eyes",
        "🚀" | "rocket" | "launch" => "launch",
        "🏆" | "trophy" => "trophy",
        "✅" | "check" | "white_check_mark" => "2705_whiteheavycheckmark",
        _ => {
            if trimmed
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
            {
                return Ok(trimmed.to_ascii_lowercase());
            }
            return Err(LiveError::bad(format!(
                "Unsupported reaction '{trimmed}'. Use a standard emoji (👍, ❤️, 😂, 😮, 😢, 😡, 🎉, 👏, 🙏, 💯, 🔥, 👀, 🚀, ✅) or Teams emotion key (e.g. like, heart, 1f389_partypopper)."
            )));
        }
    };
    Ok(mapped.to_string())
}

pub fn guess_mime_from_filename(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "pdf" => "application/pdf",
        "txt" | "log" | "md" => "text/plain",
        "csv" => "text/csv",
        "json" => "application/json",
        "zip" => "application/zip",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        _ => "application/octet-stream",
    }
}

pub fn normalize_mime_type(ct: &str) -> String {
    ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase()
}

pub fn is_supported_image_mime(ct: &str) -> bool {
    matches!(
        normalize_mime_type(ct).as_str(),
        "image/png" | "image/jpeg" | "image/jpg" | "image/gif" | "image/webp" | "image/bmp"
    )
}
