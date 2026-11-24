use crate::query::api::{self, ApiError};
use crate::timeutil::{day, hhmm};
use rusqlite::Connection;
use serde_json::Value;
use std::collections::HashMap;

fn quote_lines(text: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return vec![">".to_string()];
    }
    lines
        .into_iter()
        .map(|l| {
            if l.is_empty() {
                ">".to_string()
            } else {
                format!("> {l}")
            }
        })
        .collect()
}

fn label(a: &Value) -> String {
    let raw = a
        .get("name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| a.get("url").and_then(|v| v.as_str()))
        .unwrap_or("");
    raw.replace('[', "\\[").replace(']', "\\]")
}

pub fn md_link_dest(url: &str) -> String {
    let needs = url
        .chars()
        .any(|c| c.is_whitespace() || matches!(c, '(' | ')' | '<' | '>'));
    if !needs {
        return url.to_string();
    }
    if !url.chars().any(|c| matches!(c, '<' | '>' | '\n' | '\r')) {
        return format!("<{url}>");
    }
    let mut out = String::with_capacity(url.len() + 8);
    for c in url.chars() {
        match c {
            ' ' => out.push_str("%20"),
            '(' => out.push_str("%28"),
            ')' => out.push_str("%29"),
            '<' => out.push_str("%3C"),
            '>' => out.push_str("%3E"),
            '\n' => out.push_str("%0A"),
            '\r' => out.push_str("%0D"),
            '\t' => out.push_str("%09"),
            c => out.push(c),
        }
    }
    out
}

fn message_lines(m: &Value, history: bool) -> Vec<String> {
    let tms = m.get("timeMs").and_then(|v| v.as_i64());
    let text = m.get("text").and_then(|v| v.as_str()).unwrap_or("").trim();
    if m.get("kind").and_then(|v| v.as_str()) == Some("event") {
        return vec![format!("- *{} · {text}*", hhmm(tms))];
    }
    let mut flags = Vec::new();
    if m.get("edited").and_then(|v| v.as_bool()).unwrap_or(false) {
        flags.push("edited");
    }
    if m.get("deleted").and_then(|v| v.as_bool()).unwrap_or(false) {
        flags.push("deleted");
    }
    if matches!(
        m.get("importance")
            .and_then(|v| v.as_str())
            .map(|s| s.to_ascii_lowercase())
            .as_deref(),
        Some("high") | Some("urgent")
    ) {
        flags.push("important");
    }
    let sender = m
        .get("sender")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("Unknown");
    let mut head = format!("**{} · {sender}**", hhmm(tms));
    if !flags.is_empty() {
        head.push_str(&format!(" _({})_", flags.join(", ")));
    }
    let mut lines = vec![head];
    if let Some(subj) = m.get("subject").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
        lines.push(String::new());
        lines.push(format!("**Subject:** {subj}"));
    }
    if !text.is_empty() {
        lines.push(String::new());
        for l in text.lines() {
            lines.push(l.to_string());
        }
    }
    if let Some(atts) = m.get("attachments").and_then(|v| v.as_array()) {
        let kept: Vec<&Value> = atts
            .iter()
            .filter(|a| {
                let url = a.get("url").and_then(|v| v.as_str()).unwrap_or("");
                let kind = a.get("kind").and_then(|v| v.as_str()).unwrap_or("");
                let name = a
                    .get("name")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or(url);
                !(text.contains(url) && (kind == "link" || name == url))
            })
            .collect();
        if !kept.is_empty() {
            lines.push(String::new());
            for a in kept {
                let url = a.get("url").and_then(|v| v.as_str()).unwrap_or("");
                lines.push(format!("- [{}]({})", label(a), md_link_dest(url)));
            }
        }
    }
    if let Some(reacts) = m.get("reactions").and_then(|v| v.as_array()).filter(|r| !r.is_empty()) {
        let parts: Vec<String> = reacts
            .iter()
            .map(|r| {
                let em = r.get("emoji").and_then(|v| v.as_str()).unwrap_or("?");
                match r.get("count").and_then(|v| v.as_i64()).filter(|&c| c > 0) {
                    Some(c) => format!("{em} {c}"),
                    None => em.to_string(),
                }
            })
            .collect();
        lines.push(String::new());
        lines.push(format!("Reactions: {}", parts.join(", ")));
    }
    if history {
        if let Some(hist) = m.get("history").and_then(|v| v.as_array()) {
            for v in hist {
                let when = v
                    .get("editedAt")
                    .and_then(|x| x.as_str())
                    .or_else(|| v.get("capturedAt").and_then(|x| x.as_str()));
                let htext = v.get("text").and_then(|x| x.as_str()).unwrap_or("");
                let header = match when {
                    Some(w) => format!("*Earlier version ({w}):*\n{htext}"),
                    None => format!("*Earlier version:*\n{htext}"),
                };
                lines.push(String::new());
                lines.extend(quote_lines(&header));
            }
        }
    }
    lines
}

fn as_item(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            if i == 0 {
                if line.starts_with("- *") {
                    line.clone()
                } else {
                    format!("- {line}")
                }
            } else if line.is_empty() {
                String::new()
            } else {
                format!("  {line}")
            }
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub fn export_markdown(
    conn: &Connection,
    chat: &str,
    since: Option<&str>,
    until: Option<&str>,
    sender: Option<&str>,
    include_events: bool,
    history: bool,
    thread: Option<&str>,
) -> Result<String, ApiError> {
    let r = api::read_messages(
        conn,
        Some(chat),
        since,
        until,
        sender,
        0,
        include_events,
        thread,
        history,
        None,
        10,
    )?;
    let c = &r["chat"];
    let msgs = r["messages"].as_array().cloned().unwrap_or_default();
    let count = msgs
        .iter()
        .filter(|m| m.get("kind").and_then(|v| v.as_str()) == Some("message"))
        .count();
    let ckind = c.get("kind").and_then(|v| v.as_str()).unwrap_or("chat");
    let cid = c.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let cname = c
        .get("name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(cid);

    let mut meta = vec![
        ckind.to_string(),
        format!("`{cid}`"),
        format!("{count} message{}", if count == 1 { "" } else { "s" }),
    ];
    let times: Vec<i64> = msgs
        .iter()
        .filter_map(|m| m.get("timeMs").and_then(|v| v.as_i64()))
        .collect();
    if let (Some(&lo), Some(&hi)) = (times.iter().min(), times.iter().max()) {
        let dlo = day(Some(lo));
        let dhi = day(Some(hi));
        meta.push(if dlo == dhi { dlo } else { format!("{dlo} → {dhi}") });
    }
    let mut fparts = Vec::new();
    for (k, v) in [("since", since), ("until", until), ("from", sender), ("thread", thread)] {
        if let Some(s) = v.filter(|x| !x.is_empty()) {
            fparts.push(format!("{k}: {s}"));
        }
    }
    if include_events {
        fparts.push("with events".to_string());
    }
    if history {
        fparts.push("with edit history".to_string());
    }
    if !fparts.is_empty() {
        meta.push(fparts.join(", "));
    }

    let mut out = vec![format!("# {cname}"), String::new(), format!("_{}_", meta.join(" · "))];
    if let Some(topic) = c
        .get("topic")
        .and_then(|v| v.as_str())
        .filter(|t| !t.is_empty() && *t != cname)
    {
        out.push(String::new());
        out.push(format!("> {topic}"));
    }

    let blocks: Vec<(Option<i64>, bool, Vec<String>)> = if ckind == "channel" {
        channel_blocks(&msgs, history)
    } else {
        msgs.iter()
            .map(|m| {
                (
                    m.get("timeMs").and_then(|v| v.as_i64()),
                    m.get("kind").and_then(|v| v.as_str()) == Some("event"),
                    message_lines(m, history),
                )
            })
            .collect()
    };

    let mut cur_day = String::new();
    let mut prev_event = false;
    for (tms, is_event, lines) in blocks {
        let d = day(tms);
        if d != cur_day {
            out.push(String::new());
            out.push(format!("## {d}"));
            cur_day = d;
            prev_event = false;
        }
        if !(is_event && prev_event) {
            out.push(String::new());
        }
        out.extend(lines);
        prev_event = is_event;
    }
    if msgs.is_empty() {
        out.push(String::new());
        out.push("_No messages._".to_string());
    }
    Ok(format!("{}\n", out.join("\n").trim_end()))
}

fn channel_blocks(msgs: &[Value], history: bool) -> Vec<(Option<i64>, bool, Vec<String>)> {
    let mut roots: HashMap<String, &Value> = HashMap::new();
    let mut replies: HashMap<String, Vec<&Value>> = HashMap::new();
    let mut order: Vec<(&str, &Value)> = Vec::new();
    for m in msgs {
        if m.get("isReply").and_then(|v| v.as_bool()).unwrap_or(false) {
            let tid = m.get("threadId").and_then(|v| v.as_str()).unwrap_or("").to_string();
            replies.entry(tid).or_default().push(m);
        } else {
            let id = m.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            roots.insert(id, m);
            order.push(("root", m));
        }
    }
    for (tid, rs) in &replies {
        if !roots.contains_key(tid) {
            if let Some(first) = rs.first() {
                order.push(("orphan", first));
            }
        }
    }
    order.sort_by(|(_, a), (_, b)| {
        let ta = a.get("timeMs").and_then(|v| v.as_i64()).unwrap_or(0);
        let tb = b.get("timeMs").and_then(|v| v.as_i64()).unwrap_or(0);
        let ia = a.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let ib = b.get("id").and_then(|v| v.as_str()).unwrap_or("");
        ta.cmp(&tb).then_with(|| ia.cmp(ib))
    });
    let mut blocks = Vec::new();
    for (what, m) in order {
        let (mut lines, rs) = if what == "root" {
            let l = message_lines(m, history);
            let mid = m.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let tid = m
                .get("threadId")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(mid);
            let rs = if tid == mid {
                replies.get(tid).cloned().unwrap_or_default()
            } else {
                Vec::new()
            };
            (l, rs)
        } else {
            let tid = m.get("threadId").and_then(|v| v.as_str()).unwrap_or("");
            (
                vec!["*↳ Replies to an earlier post*".to_string()],
                replies.get(tid).cloned().unwrap_or_default(),
            )
        };
        if !rs.is_empty() {
            lines.push(String::new());
            for rep in rs {
                lines.extend(as_item(&message_lines(rep, history)));
            }
        }
        blocks.push((
            m.get("timeMs").and_then(|v| v.as_i64()),
            m.get("kind").and_then(|v| v.as_str()) == Some("event"),
            lines,
        ));
    }
    blocks
}
