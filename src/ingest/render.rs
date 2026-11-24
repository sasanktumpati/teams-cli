use base64::Engine;
use regex::Regex;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

static REC_STATUS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"RecordingStatus status="(\w+)""#).unwrap());
static SWIFT_B64_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"<Swift b64="([^"]+)""#).unwrap());
static HEX_EMOJI_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([0-9a-f]{4,6})(?:_|$)").unwrap());
static ATTR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"([a-zA-Z0-9_:-]+)(?:\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'=<>`]+)))?"#).unwrap());

#[inline]
fn is_html_ws(c: char) -> bool {
    matches!(
        c,
        ' ' | '\t' | '\r' | '\n' | '\x0c' | '\x0b' | '\u{00a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
    )
}

fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for c in s.chars() {
        if is_html_ws(c) {
            if !in_ws {
                out.push(' ');
                in_ws = true;
            }
        } else {
            out.push(c);
            in_ws = false;
        }
    }
    out
}

fn collapse_multi_space(s: &str, out: &mut String) {
    let mut prev_space = false;
    for c in s.chars() {
        if c == ' ' {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(c);
            prev_space = false;
        }
    }
}

fn strip_zero_width(s: &str) -> String {
    if !s.bytes().any(|b| b >= 0xE2) {
        return s.to_string();
    }
    s.chars()
        .filter(|&c| !matches!(c, '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{2060}' | '\u{feff}'))
        .collect()
}

pub fn html_unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'&' {
            if let Some(semi) = s[i + 1..].find(';').filter(|&d| d > 0 && d <= 12) {
                let ent = &s[i + 1..i + 1 + semi];
                if let Some(decoded) = decode_entity(ent) {
                    out.push_str(&decoded);
                    i += semi + 2;
                    continue;
                }
            }
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn decode_entity(ent: &str) -> Option<String> {
    match ent {
        "amp" => Some("&".into()),
        "lt" => Some("<".into()),
        "gt" => Some(">".into()),
        "quot" => Some("\"".into()),
        "apos" => Some("'".into()),
        "nbsp" => Some("\u{00a0}".into()),
        "ndash" => Some("–".into()),
        "mdash" => Some("—".into()),
        "lsquo" => Some("‘".into()),
        "rsquo" => Some("’".into()),
        "ldquo" => Some("“".into()),
        "rdquo" => Some("”".into()),
        "hellip" => Some("…".into()),
        "copy" => Some("©".into()),
        _ => {
            if let Some(hex_str) = ent.strip_prefix("#x").or_else(|| ent.strip_prefix("#X")) {
                u32::from_str_radix(hex_str, 16)
                    .ok()
                    .and_then(char::from_u32)
                    .map(|c| c.to_string())
            } else if let Some(dec_str) = ent.strip_prefix('#') {
                dec_str
                    .parse::<u32>()
                    .ok()
                    .and_then(char::from_u32)
                    .map(|c| c.to_string())
            } else {
                None
            }
        }
    }
}

fn normalize(text: &str) -> String {
    let mut joined = String::with_capacity(text.len());
    let mut in_code = false;
    let mut first_line = true;
    for line in text.split('\n') {
        if !first_line {
            joined.push('\n');
        }
        first_line = false;
        if line.trim().starts_with("```") {
            in_code = !in_code;
            joined.push_str(line.trim());
            continue;
        }
        if in_code {
            joined.push_str(line.trim_end());
            continue;
        }
        let leading_spaces = line.bytes().take_while(|&b| b == b' ').count();
        if line[leading_spaces..].starts_with("- ") {
            joined.push_str(&line[..leading_spaces]);
        }
        collapse_multi_space(line.trim(), &mut joined);
    }
    let mut out = String::with_capacity(joined.len());
    let mut nl_run = 0usize;
    for c in joined.chars() {
        if c == '\n' {
            nl_run += 1;
            if nl_run <= 2 {
                out.push('\n');
            }
        } else {
            nl_run = 0;
            out.push(c);
        }
    }
    out.trim().to_string()
}

struct Frame {
    kind: &'static str,
    parts: Vec<String>,
    links: Vec<(String, String, usize)>,
    last_mention: Option<(String, usize)>,
}

impl Frame {
    fn new(kind: &'static str) -> Self {
        Frame {
            kind,
            parts: Vec::new(),
            links: Vec::new(),
            last_mention: None,
        }
    }
}

fn find_tag_end(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'>' => return Some(i),
            b'=' => {
                let mut j = i + 1;
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j < b.len() && (b[j] == b'"' || b[j] == b'\'') {
                    let q = b[j];
                    match b[j + 1..].iter().position(|&c| c == q) {
                        Some(rel) => i = j + 1 + rel + 1,
                        None => return s.find('>'),
                    }
                    continue;
                }
                i = j;
            }
            _ => i += 1,
        }
    }
    None
}

fn is_block(tag: &str) -> bool {
    matches!(
        tag,
        "p" | "div" | "tr" | "table" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "hr" | "section"
    )
}

fn image_text(attrs: &HashMap<String, String>, itemtype: &str) -> String {
    let alt = attrs.get("alt").map(String::as_str).unwrap_or("");
    let class = attrs.get("class").map(String::as_str).unwrap_or("");
    if itemtype.contains("Emoji") || class.contains("emoticon") {
        return alt.to_string();
    }
    if !alt.is_empty() && !alt.is_ascii() && alt.chars().count() <= 4 {
        return alt.to_string();
    }
    if itemtype.contains("AMSImage") || alt.is_empty() || alt.eq_ignore_ascii_case("image") {
        return "[image]".to_string();
    }
    format!("[image: {alt}]")
}

fn link_text(text: &str, href: &str, itemtype: &str) -> String {
    if itemtype.contains("/HyperLink/Files") || itemtype.contains("/File") {
        let label = if text.is_empty() { href } else { text };
        return format!("[file: {label}]");
    }
    if !href.starts_with("http://") && !href.starts_with("https://") {
        return text.to_string();
    }
    if text.is_empty() || text == href || text.ends_with('…') || text.ends_with("...") {
        return href.to_string();
    }
    if text.trim_end_matches('/') == href.trim_end_matches('/') {
        return text.to_string();
    }
    format!("{text} ({href})")
}

fn parse_attrs(raw: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for cap in ATTR_RE.captures_iter(raw) {
        let k = cap[1].to_ascii_lowercase();
        let v = cap
            .get(2)
            .or_else(|| cap.get(3))
            .or_else(|| cap.get(4))
            .map(|m| html_unescape(m.as_str()))
            .unwrap_or_default();
        map.insert(k, v);
    }
    map
}

fn push_nl(buf: &mut Vec<String>) {
    for part in buf.iter().rev() {
        if !part.is_empty() {
            if !part.ends_with('\n') {
                buf.push("\n".to_string());
            }
            return;
        }
    }
}

pub fn html_to_text(content: &str, mentions: &HashMap<String, String>) -> String {
    if content.is_empty() {
        return String::new();
    }
    if !content.contains('<') {
        let clean = strip_zero_width(&html_unescape(content)).replace('\u{00a0}', " ");
        return normalize(&clean);
    }

    let mut frames: Vec<Frame> = vec![Frame::new("root")];
    let mut pre: usize = 0;
    let mut inline_code: usize = 0;
    let mut skip: usize = 0;
    let mut list_depth: usize = 0;
    let mut spans: Vec<Option<String>> = Vec::new();

    let close_blockquote = |frames: &mut Vec<Frame>| {
        if frames.len() <= 1 {
            return;
        }
        let frame = frames.pop().unwrap();
        let text = normalize(&frame.parts.concat());
        if text.is_empty() {
            return;
        }
        let mut lines: Vec<String> = text.split('\n').map(String::from).collect();
        if frame.kind == "reply" {
            let head = lines[0].trim().to_string();
            let rest: Vec<&str> = lines[1..].iter().map(|l| l.trim()).filter(|l| !l.is_empty()).collect();
            let combined = if rest.is_empty() {
                head
            } else {
                format!("{head}: {}", rest.join(" "))
            };
            lines = vec![combined];
        } else if frame.kind == "forward" {
            let mut next = vec!["Forwarded:".to_string()];
            next.extend(lines);
            lines = next;
        }
        let buf = &mut frames.last_mut().unwrap().parts;
        push_nl(buf);
        let quoted: Vec<String> = lines
            .into_iter()
            .map(|l| {
                if l.is_empty() {
                    ">".to_string()
                } else {
                    format!("> {l}")
                }
            })
            .collect();
        buf.push(format!("{}\n", quoted.join("\n")));
    };

    let bytes = content.as_bytes();
    let mut pos = 0usize;
    while pos < bytes.len() {
        if bytes[pos] == b'<' {
            if content[pos..].starts_with("<!--") {
                if let Some(end) = content[pos + 4..].find("-->") {
                    pos += 4 + end + 3;
                } else {
                    pos += 4;
                }
                continue;
            }
            if let Some(gt_rel) = find_tag_end(&content[pos + 1..]) {
                let inside = content[pos + 1..pos + 1 + gt_rel].trim();
                pos += gt_rel + 2;
                if inside.is_empty() || inside.starts_with('!') || inside.starts_with('?') {
                    continue;
                }
                let is_end = inside.starts_with('/');
                let self_closing = !is_end && inside.ends_with('/');
                let body = inside.trim_start_matches('/').trim_end_matches('/').trim();
                let (tag_raw, attr_raw) = match body.find(|c: char| c.is_ascii_whitespace()) {
                    Some(idx) => (&body[..idx], &body[idx..]),
                    None => (body, ""),
                };
                let tag = tag_raw.to_ascii_lowercase();
                if is_end {
                    match tag.as_str() {
                        "script" | "style" => skip = skip.saturating_sub(1),
                        "blockquote" => close_blockquote(&mut frames),
                        "pre" if pre > 0 => {
                            pre -= 1;
                            let buf = &mut frames.last_mut().unwrap().parts;
                            push_nl(buf);
                            buf.push("```\n".to_string());
                        }
                        "code" if pre == 0 && inline_code > 0 => {
                            inline_code -= 1;
                            frames.last_mut().unwrap().parts.push("`".to_string());
                        }
                        "ul" | "ol" => {
                            list_depth = list_depth.saturating_sub(1);
                            push_nl(&mut frames.last_mut().unwrap().parts);
                        }
                        "span" => {
                            if let Some(Some(mri)) = spans.pop() {
                                let frame = frames.last_mut().unwrap();
                                frame.last_mention = Some((mri, frame.parts.len()));
                            }
                        }
                        "a" => {
                            let frame = frames.last_mut().unwrap();
                            if let Some((href, itemtype, start)) = frame.links.pop() {
                                let buf = &mut frame.parts;
                                let raw_t: String = if start <= buf.len() {
                                    buf.drain(start..).collect()
                                } else {
                                    String::new()
                                };
                                let t = collapse_ws(&raw_t).trim().to_string();
                                buf.push(link_text(&t, &href, &itemtype));
                            }
                        }
                        t if is_block(t) => push_nl(&mut frames.last_mut().unwrap().parts),
                        _ => {}
                    }
                } else {
                    match tag.as_str() {
                        "script" | "style" => {
                            if !self_closing {
                                skip += 1;
                            }
                        }
                        "blockquote" => {
                            let attrs = if attr_raw.is_empty() {
                                HashMap::new()
                            } else {
                                parse_attrs(attr_raw)
                            };
                            let itemtype = attrs.get("itemtype").map(String::as_str).unwrap_or("");
                            let kind = if itemtype.ends_with("/Reply") {
                                "reply"
                            } else if itemtype.ends_with("/Forward") {
                                "forward"
                            } else {
                                "quote"
                            };
                            frames.push(Frame::new(kind));
                            if self_closing {
                                close_blockquote(&mut frames);
                            }
                        }
                        "pre" => {
                            pre += 1;
                            let buf = &mut frames.last_mut().unwrap().parts;
                            push_nl(buf);
                            buf.push("```\n".to_string());
                        }
                        "code" if pre == 0 => {
                            inline_code += 1;
                            frames.last_mut().unwrap().parts.push("`".to_string());
                        }
                        "br" => frames.last_mut().unwrap().parts.push("\n".to_string()),
                        "ul" | "ol" => {
                            list_depth += 1;
                            push_nl(&mut frames.last_mut().unwrap().parts);
                        }
                        "li" => {
                            let buf = &mut frames.last_mut().unwrap().parts;
                            push_nl(buf);
                            let indent = "  ".repeat(list_depth.saturating_sub(1));
                            buf.push(format!("{indent}- "));
                        }
                        "td" | "th" => frames.last_mut().unwrap().parts.push(" | ".to_string()),
                        t if is_block(t) => push_nl(&mut frames.last_mut().unwrap().parts),
                        "img" => {
                            let attrs = if attr_raw.is_empty() {
                                HashMap::new()
                            } else {
                                parse_attrs(attr_raw)
                            };
                            let itemtype = attrs.get("itemtype").map(String::as_str).unwrap_or("");
                            frames.last_mut().unwrap().parts.push(image_text(&attrs, itemtype));
                        }
                        "span" => {
                            let mut mri = None;
                            if !attr_raw.is_empty() {
                                let attrs = parse_attrs(attr_raw);
                                let itemtype = attrs.get("itemtype").map(String::as_str).unwrap_or("");
                                if itemtype.ends_with("/Mention") {
                                    let itemid = attrs.get("itemid").map(String::as_str).unwrap_or("");
                                    let resolved =
                                        mentions.get(itemid).cloned().unwrap_or_else(|| format!("id:{itemid}"));
                                    let frame = frames.last_mut().unwrap();
                                    let buf = &mut frame.parts;
                                    let continuing = match &frame.last_mention {
                                        Some((prev_mri, prev_pos))
                                            if *prev_mri == resolved && *prev_pos <= buf.len() =>
                                        {
                                            buf[*prev_pos..].concat().trim().is_empty()
                                        }
                                        _ => false,
                                    };
                                    if !continuing {
                                        buf.push("@".to_string());
                                    }
                                    mri = Some(resolved);
                                }
                            }
                            if !self_closing {
                                spans.push(mri);
                            }
                        }
                        "a" if !self_closing => {
                            let attrs = if attr_raw.is_empty() {
                                HashMap::new()
                            } else {
                                parse_attrs(attr_raw)
                            };
                            let href = attrs.get("href").cloned().unwrap_or_default();
                            let itemtype = attrs.get("itemtype").cloned().unwrap_or_default();
                            let frame = frames.last_mut().unwrap();
                            let start = frame.parts.len();
                            frame.links.push((href, itemtype, start));
                        }
                        _ => {}
                    }
                }
                continue;
            }
        }
        let next_lt = content[pos..].find('<').map(|d| pos + d).unwrap_or(bytes.len());
        let raw_seg = if next_lt == pos {
            pos += 1;
            "<"
        } else {
            let s = &content[pos..next_lt];
            pos = next_lt;
            s
        };
        if skip == 0 {
            let unescaped = strip_zero_width(&html_unescape(raw_seg));
            let piece = if pre > 0 {
                unescaped.replace('\u{00a0}', " ")
            } else {
                collapse_ws(&unescaped)
            };
            frames.last_mut().unwrap().parts.push(piece);
        }
    }

    while frames.len() > 1 {
        close_blockquote(&mut frames);
    }
    normalize(&frames[0].parts.concat())
}

fn xml_values(content: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = content;
    while let Some(s) = rest.find(&open) {
        let after = &rest[s + open.len()..];
        if let Some(e) = after.find(&close) {
            out.push(after[..e].to_string());
            rest = &after[e + close.len()..];
        } else {
            break;
        }
    }
    out
}

fn names_list(mris: &[String], name_of: &dyn Fn(&str) -> String) -> String {
    let mut seen: Vec<String> = Vec::new();
    for m in mris {
        let n = name_of(m);
        if !seen.contains(&n) {
            seen.push(n);
        }
    }
    if seen.is_empty() {
        "someone".to_string()
    } else {
        seen.join(", ")
    }
}

pub fn render_event(message_type: &str, content: &str, name_of: &dyn Fn(&str) -> String) -> Option<String> {
    match message_type {
        "ThreadActivity/AddMember" => {
            let init = xml_values(content, "initiator");
            let targets = xml_values(content, "target");
            Some(format!(
                "{} added {}",
                names_list(&init, name_of),
                names_list(&targets, name_of)
            ))
        }
        "ThreadActivity/DeleteMember" => {
            let init = xml_values(content, "initiator");
            let targets = xml_values(content, "target");
            if !init.is_empty() && targets == init {
                Some(format!("{} left", names_list(&init, name_of)))
            } else {
                Some(format!(
                    "{} removed {}",
                    names_list(&init, name_of),
                    names_list(&targets, name_of)
                ))
            }
        }
        "ThreadActivity/MemberJoined" | "ThreadActivity/MemberLeft" => {
            let d: Value = serde_json::from_str(content).unwrap_or(Value::Null);
            let who: Vec<String> = d
                .get("members")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|m| m.get("id").and_then(|v| v.as_str()).map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let verb = if message_type.ends_with("Joined") {
                "joined"
            } else {
                "left"
            };
            Some(format!("{} {verb}", names_list(&who, name_of)))
        }
        "ThreadActivity/TopicUpdate" => {
            let init = xml_values(content, "initiator");
            let val = xml_values(content, "value")
                .first()
                .map(|s| html_unescape(s))
                .unwrap_or_default();
            Some(format!(
                "{} renamed the conversation to “{val}”",
                names_list(&init, name_of)
            ))
        }
        "ThreadActivity/TopicTopicUpdated" | "ThreadActivity/TopicDescriptionUpdated" => {
            let d: Value = serde_json::from_str(content).unwrap_or(Value::Null);
            let what = if message_type.contains("TopicUpdated") {
                "name"
            } else {
                "description"
            };
            let user = d.get("user").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let new_val = d.get("newValue").and_then(|v| v.as_str()).unwrap_or("");
            Some(format!(
                "{} changed the {what} to “{new_val}”",
                names_list(&[user], name_of)
            ))
        }
        "ThreadActivity/PinnedItemsUpdate" => {
            let d: Value = serde_json::from_str(content).unwrap_or(Value::Null);
            let user = d.get("userId").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let op = d.get("operation").and_then(|v| v.as_str()).unwrap_or("pinned");
            Some(format!("{} {op} a message", names_list(&[user], name_of)))
        }
        "Event/Call" => {
            let kind = xml_values(content, "callEventType");
            let label = match kind.first().map(String::as_str) {
                Some("callStarted") => "Call started",
                Some("callEnded") => "Call ended",
                _ => "Call",
            };
            Some(label.to_string())
        }
        "RichText/Media_CallRecording" | "RichText/Media_CallLogRecording" => {
            if let Some(cap) = REC_STATUS_RE.captures(content) {
                if &cap[1] != "Success" {
                    return None;
                }
            }
            let title = xml_values(content, "Title")
                .first()
                .map(|s| html_unescape(s))
                .unwrap_or_default();
            if title.is_empty() {
                Some("Recording available".to_string())
            } else {
                Some(format!("Recording available: {title}"))
            }
        }
        "RichText/Media_CallTranscript" | "RichText/Media_CallLogTranscript" => {
            Some("Transcript available".to_string())
        }
        t if t.starts_with("ThreadActivity/") => None,
        t => Some(format!("[{t}]")),
    }
}

pub fn render_activity(a: &Value) -> String {
    let who = a
        .get("sourceUserImDisplayName")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("Someone");
    let kind = a.get("activityType").and_then(|v| v.as_str()).unwrap_or("");
    let sub = a.get("activitySubtype").and_then(|v| v.as_str()).unwrap_or("");
    if kind == "call" && sub == "missedCall" {
        return format!("Missed call from {who}");
    }
    let verb = if (kind == "reactionInChat" || kind == "reaction") && !sub.is_empty() {
        format!("reacted {}", reaction_emoji(sub))
    } else {
        match kind {
            "mention" => "mentioned you",
            "reply" | "replyToReply" => "replied",
            "reactionInChat" | "reaction" => "reacted",
            "call" => "called",
            "teamMembershipChange" => "changed team membership",
            "follow" => "posted in a followed channel",
            "" => "activity",
            other => other,
        }
        .to_string()
    };
    let preview = a.get("messagePreview").and_then(|v| v.as_str()).unwrap_or("").trim();
    if preview.is_empty() {
        format!("{who} {verb}")
    } else {
        format!("{who} {verb}: {preview}")
    }
}

pub fn render_card(content: &str) -> String {
    let Some(cap) = SWIFT_B64_RE.captures(content) else {
        return "[card]".to_string();
    };
    let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(&cap[1]) else {
        return "[card]".to_string();
    };
    let Ok(data) = serde_json::from_slice::<Value>(&raw) else {
        return "[card]".to_string();
    };
    let mut texts: Vec<String> = Vec::new();
    fn walk(node: &Value, texts: &mut Vec<String>) {
        match node {
            Value::Object(map) => {
                let typ = map.get("type").and_then(|v| v.as_str());
                if matches!(typ, Some("TextBlock") | Some("TextRun")) {
                    if let Some(t) = map.get("text").and_then(|v| v.as_str()) {
                        texts.push(t.to_string());
                    }
                }
                if typ.is_none() {
                    for k in ["title", "subtitle"] {
                        if let Some(t) = map.get(k).and_then(|v| v.as_str()) {
                            texts.push(t.to_string());
                        }
                    }
                }
                for v in map.values() {
                    walk(v, texts);
                }
            }
            Value::Array(arr) => {
                for v in arr {
                    walk(v, texts);
                }
            }
            _ => {}
        }
    }
    walk(&data, &mut texts);
    let mut seen = HashSet::new();
    let mut unique = Vec::new();
    for t in texts {
        let tr = t.trim().to_string();
        if !tr.is_empty() && seen.insert(tr.clone()) {
            unique.push(tr);
        }
    }
    if unique.is_empty() {
        "[card]".to_string()
    } else {
        format!("[card]\n{}", unique.join("\n"))
    }
}

pub fn reaction_emoji(key: &str) -> String {
    match key {
        "like" | "yes" => "\u{1f44d}".into(),
        "heart" => "❤️".into(),
        "laugh" => "\u{1f606}".into(),
        "surprised" => "\u{1f62e}".into(),
        "sad" => "\u{1f622}".into(),
        "angry" => "\u{1f621}".into(),
        "no" => "\u{1f44e}".into(),
        "clap" => "\u{1f44f}".into(),
        "fire" => "\u{1f525}".into(),
        "pray" => "\u{1f64f}".into(),
        "100" => "\u{1f4af}".into(),
        "salute" => "\u{1fae1}".into(),
        "rofl" => "\u{1f923}".into(),
        "smile" => "\u{1f642}".into(),
        "eyes" => "\u{1f440}".into(),
        other => {
            if let Some(cap) = HEX_EMOJI_RE.captures(other) {
                if let Ok(cp) = u32::from_str_radix(&cap[1], 16) {
                    if let Some(ch) = char::from_u32(cp) {
                        return ch.to_string();
                    }
                }
            }
            other.to_string()
        }
    }
}
