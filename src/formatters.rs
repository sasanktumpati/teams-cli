use crate::json_codec;
use crate::timeutil::short;
use serde_json::Value;
use std::collections::HashMap;
use std::fmt;

pub fn quote(s: &str) -> String {
    let q = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(q);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if c == q => {
                out.push('\\');
                out.push(c);
            }
            c if !is_printable(c) => {
                let n = c as u32;
                if n < 0x100 {
                    out.push_str(&format!("\\x{n:02x}"));
                } else if n < 0x10000 {
                    out.push_str(&format!("\\u{n:04x}"));
                } else {
                    out.push_str(&format!("\\U{n:08x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push(q);
    out
}

pub struct Quoted<'a>(pub &'a str);

impl fmt::Display for Quoted<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&quote(self.0))
    }
}

fn is_printable(c: char) -> bool {
    if c == ' ' {
        return true;
    }
    if c.is_control() {
        return false;
    }
    let n = c as u32;
    !matches!(
        n,
        0x00A0 | 0x1680 | 0x2000..=0x200A | 0x2028 | 0x2029 | 0x202F | 0x205F | 0x3000
        | 0x00AD | 0x0600..=0x0605 | 0x061C | 0x06DD | 0x070F | 0x0890 | 0x0891 | 0x08E2 | 0x180E
        | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x2064 | 0x2066..=0x206F | 0xFEFF
        | 0xFFF9..=0xFFFB | 0x110BD | 0x110CD | 0x13430..=0x1343F | 0x1BCA0..=0x1BCA3
        | 0x1D173..=0x1D17A | 0xE0001 | 0xE0020..=0xE007F
        | 0xE000..=0xF8FF | 0xF0000..=0xFFFFD | 0x100000..=0x10FFFD
        | 0xFDD0..=0xFDEF | 0xFFFE | 0xFFFF
    )
}

pub fn print_json(obj: &Value) {
    println!("{}", json_codec::to_pretty(obj));
}

pub fn indent(text: &str, prefix: &str) -> String {
    text.split('\n')
        .map(|l| format!("{prefix}{l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn line_preview(text: Option<&str>, width: usize) -> String {
    let t = text.unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" ");
    if t.chars().count() <= width {
        t
    } else {
        let mut s: String = t.chars().take(width.saturating_sub(1)).collect();
        s.push('…');
        s
    }
}

pub fn duration(seconds: Option<i64>) -> String {
    let Some(s) = seconds else { return "-".to_string() };
    let h = s / 3600;
    let rem = s % 3600;
    if h > 0 {
        format!("{h}h{:02}m", rem / 60)
    } else {
        format!("{}m{:02}s", rem / 60, rem % 60)
    }
}

pub fn print_more(r: &Value, key: &str, noun: &str) {
    let shown = r.get(key).and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
    let matched = r.get("matched").and_then(|v| v.as_i64()).unwrap_or(0);
    let trunc = r.get("truncated").and_then(|v| v.as_bool()).unwrap_or(false);
    let hint = if trunc { " (use --limit 0 for all)" } else { "" };
    println!("\n# {shown} of {matched} {noun}{hint}");
}

pub fn note_senders(senders: Option<&Value>, asked: Option<&str>) {
    let Some(arr) = senders.and_then(|v| v.as_array()) else {
        return;
    };
    if arr.len() > 1 {
        let names: Vec<String> = arr
            .iter()
            .take(8)
            .map(|s| {
                let n = s.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                let c = s.get("messages").and_then(|v| v.as_i64()).unwrap_or(0);
                format!("{n} ({c})")
            })
            .collect();
        eprintln!(
            "# --from {} matched {} people: {}. Use a fuller name or their email to pick one.",
            Quoted(asked.unwrap_or("")),
            arr.len(),
            names.join(", ")
        );
    }
}

pub fn fmt_message(m: &Value, prefix: &str, chat: Option<&str>) -> String {
    let pad = format!("{}    ", " ".repeat(prefix.chars().count()));
    let when = short(m.get("timeMs").and_then(|v| v.as_i64()));
    let text = m.get("text").and_then(|v| v.as_str()).unwrap_or("");
    if m.get("kind").and_then(|v| v.as_str()) == Some("event") {
        return format!("{prefix}[{when}] · {text}");
    }
    let edited_on = m.get("edited").and_then(|v| v.as_bool()).unwrap_or(false);
    let deleted_on = m.get("deleted").and_then(|v| v.as_bool()).unwrap_or(false);
    let mut flags = Vec::new();
    if edited_on {
        let ed = m
            .get("editedAt")
            .and_then(|v| v.as_str())
            .filter(|s| s.chars().count() >= 16)
            .map(|s| {
                format!(
                    "edited {}",
                    s.chars().skip(5).take(11).collect::<String>().replace('T', " ")
                )
            })
            .unwrap_or_else(|| "edited".to_string());
        flags.push(ed);
    }
    if deleted_on {
        let del = match m.get("deletedBy").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
            Some(by) => format!("deleted by {by}"),
            None => "deleted".to_string(),
        };
        flags.push(del);
    }
    if matches!(
        m.get("importance")
            .and_then(|v| v.as_str())
            .map(|s| s.to_ascii_lowercase())
            .as_deref(),
        Some("high") | Some("urgent")
    ) {
        flags.push("important".to_string());
    }
    let sender = m
        .get("sender")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("?");
    let mut head = format!("{prefix}[{when}] {sender}");
    if let Some(c) = chat.filter(|s| !s.is_empty()) {
        head.push_str(&format!("  @ {c}"));
    }
    if !flags.is_empty() {
        head.push_str(&format!("  ({})", flags.join(", ")));
    }
    let mut body = m
        .get("snippet")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or((!text.is_empty()).then_some(text))
        .unwrap_or(if deleted_on {
            "(deleted before it was archived)"
        } else {
            "(no text)"
        })
        .to_string();
    if let Some(subj) = m.get("subject").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
        body = format!("Subject: {subj}\n{body}");
    }
    let mut lines = vec![head, indent(&body, &pad)];
    if let Some(atts) = m.get("attachments").and_then(|v| v.as_array()) {
        for a in atts {
            let kind = a.get("kind").and_then(|v| v.as_str()).unwrap_or("file");
            let url = a.get("url").and_then(|v| v.as_str()).unwrap_or("");
            if kind == "link" && text.contains(url) {
                continue;
            }
            let name = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let label = if kind != "image" && !name.is_empty() && name != url {
                format!(" {name}")
            } else {
                String::new()
            };
            lines.push(format!("{pad}[{kind}]{label}  {url}"));
        }
    }
    if let Some(reacts) = m.get("reactions").and_then(|v| v.as_array()).filter(|r| !r.is_empty()) {
        let s: Vec<String> = reacts
            .iter()
            .map(|r| {
                format!(
                    "{} {}",
                    display_value(r.get("emoji").unwrap_or(&Value::Null)),
                    display_value(r.get("count").unwrap_or(&Value::Null))
                )
            })
            .collect();
        lines.push(format!("{pad}{}", s.join("  ")));
    }
    if let Some(hist) = m.get("history").and_then(|v| v.as_array()) {
        for v in hist {
            let seen = v
                .get("capturedAt")
                .and_then(|x| x.as_str())
                .map(|s| s.chars().take(16).collect::<String>().replace('T', " "))
                .unwrap_or_default();
            let ht = v
                .get("text")
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("(empty)");
            lines.push(format!("{pad}~ earlier version (archived {seen}):"));
            lines.push(indent(ht, &format!("{pad}  ")));
        }
    }
    lines.join("\n")
}

pub fn print_threaded(msgs: &[Value]) {
    print!("{}", format_threaded(msgs));
}

pub fn format_threaded(msgs: &[Value]) -> String {
    let mut out = String::new();
    let mut threads: HashMap<String, Vec<&Value>> = HashMap::new();
    for m in msgs {
        let tid = m
            .get("threadId")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .or_else(|| m.get("id").and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string();
        threads.entry(tid).or_default().push(m);
    }
    let mut groups: Vec<Vec<&Value>> = threads.into_values().collect();
    let key = |g: &Vec<&Value>| -> (i64, String) {
        let first = g.first();
        (
            first
                .and_then(|m| m.get("timeMs"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0),
            first
                .and_then(|m| m.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        )
    };
    groups.sort_by_cached_key(key);
    for items in groups {
        if items
            .first()
            .and_then(|m| m.get("isReply"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            out.push_str("… replies to a thread that started earlier\n");
        }
        for m in items {
            let pfx = if m.get("isReply").and_then(|v| v.as_bool()).unwrap_or(false) {
                "  ↳ "
            } else {
                ""
            };
            out.push_str(&fmt_message(m, pfx, None));
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

pub fn print_items(items: &[Value], time_key: &str, extra: impl Fn(&Value) -> String) {
    for x in items {
        let when = short(x.get(time_key).and_then(|v| v.as_i64()));
        let sender = x
            .get("sender")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("?");
        let where_s = x
            .get("chatName")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .or_else(|| x.get("chatId").and_then(|v| v.as_str()))
            .unwrap_or("?");
        let mid = x.get("messageId").and_then(|v| v.as_str()).unwrap_or("");
        println!("[{when}] {sender}  @ {where_s}  [{mid}]{}", extra(x));
        let mut text = x
            .get("text")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("(no text)")
            .to_string();
        if let Some(subj) = x.get("subject").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
            text = format!("Subject: {subj}\n{text}");
        }
        println!("{}", indent(&text, "    "));
    }
}

pub fn print_error_details(details: &Value) {
    if let Some(chats) = details.get("chats").and_then(|v| v.as_array()) {
        eprintln!("teams: candidate chats (pass one with --chat or as chatId/msgId):");
        for c in chats {
            if let Some(s) = c.as_str() {
                eprintln!("  {s}");
            } else if let Some(obj) = c.as_object() {
                let n = obj.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let id = obj.get("id").map(display_value).unwrap_or_else(|| "null".into());
                eprintln!("  {}", format!("{n}  [{id}]").trim());
            } else {
                eprintln!("  {}", display_value(c));
            }
        }
        if let Some(obj) = details.as_object() {
            let rest: serde_json::Map<String, Value> = obj
                .iter()
                .filter(|(k, _)| *k != "chats")
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            if !rest.is_empty() {
                eprintln!("teams: {}", json_codec::to_string(&rest));
            }
        }
        return;
    }
    eprintln!("teams: {}", json_codec::to_string(details));
}

fn display_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n
            .as_f64()
            .filter(|_| n.is_f64())
            .map(json_codec::format_float)
            .unwrap_or_else(|| n.to_string()),
        other => json_codec::to_string(other),
    }
}

#[cfg(test)]
mod tests {
    use super::quote;

    #[test]
    fn quote_escapes_and_chooses_delimiters() {
        assert_eq!(quote("nobody-here"), "'nobody-here'");
        assert_eq!(quote("it's"), "\"it's\"");
        assert_eq!(quote("it's \"x\""), "'it\\'s \"x\"'");
        assert_eq!(quote("a\\b\n\t\r"), "'a\\\\b\\n\\t\\r'");
        assert_eq!(quote("\u{7}\u{a0}\u{200b}\u{1F680}é"), "'\\x07\\xa0\\u200b\u{1F680}é'");
        assert_eq!(quote(""), "''");
    }
}
