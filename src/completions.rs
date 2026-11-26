use crate::daemon;
use crate::paths;
use crate::storage::{backups, db};
use crate::update;
use clap::{Arg, ArgAction, Command, ValueHint};
use clap_complete::Shell;
use rusqlite::{Connection, OpenFlags};
use std::collections::{HashMap, HashSet};

const MAX_ITEMS: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileMode {
    NoFiles,
    Files,
    Dirs,
}

impl FileMode {
    pub fn directive(self) -> &'static str {
        match self {
            FileMode::NoFiles => ":nofiles",
            FileMode::Files => ":files",
            FileMode::Dirs => ":dirs",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionResult {
    pub items: Vec<(String, String)>,
    pub file_mode: FileMode,
}

fn sanitize(s: &str) -> String {
    s.replace(['\t', '\r', '\n'], " ").trim().to_string()
}

fn open_ro_fast() -> Option<Connection> {
    let p = paths::archive_path();
    if !p.is_file() {
        return None;
    }
    let conn =
        Connection::open_with_flags(&p, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).ok()?;
    conn.execute_batch(
        "PRAGMA busy_timeout = 200;
         PRAGMA query_only = ON;
         PRAGMA cache_size = -512;",
    )
    .ok()?;
    Some(conn)
}

fn complete_chats() -> Vec<(String, String)> {
    let Some(conn) = open_ro_fast() else {
        return Vec::new();
    };
    let sql = format!(
        "SELECT c.id, c.name, c.kind, {} AS message_count, {} AS activity \
         FROM conversations c WHERE c.kind != 'system' \
         ORDER BY (message_count > 0) DESC, activity DESC LIMIT {MAX_ITEMS}",
        db::MESSAGE_COUNT_SQL,
        db::ACTIVITY_SQL
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?,
        ))
    }) else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for row in rows.flatten() {
        let (id, name_opt, kind, count) = row;
        let clean_name = name_opt.as_deref().map(sanitize).filter(|s| !s.is_empty());
        let (val, desc) = match clean_name {
            Some(ref n) if seen.insert(n.clone()) => (n.clone(), format!("{kind}, {count} msgs")),
            Some(ref n) => {
                seen.insert(id.clone());
                (id, format!("{n} ({kind}, {count} msgs)"))
            }
            None => {
                if !seen.insert(id.clone()) {
                    continue;
                }
                (id, format!("{kind}, {count} msgs"))
            }
        };
        out.push((val, desc));
    }
    out
}

fn complete_people() -> Vec<(String, String)> {
    let Some(conn) = open_ro_fast() else {
        return Vec::new();
    };
    let sql = format!(
        "SELECT p.mri, p.name, p.email, p.job_title, \
                (SELECT COUNT(*) FROM messages m WHERE m.sender_mri = p.mri) AS sent \
         FROM people p WHERE COALESCE(p.kind, '') != 'bot' \
         ORDER BY sent DESC, p.name COLLATE NOCASE LIMIT {MAX_ITEMS}"
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, i64>(4)?,
        ))
    }) else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for row in rows.flatten() {
        let (mri, name_opt, email_opt, title_opt, sent) = row;
        let name = name_opt.as_deref().map(sanitize).filter(|s| !s.is_empty());
        let email = email_opt.as_deref().map(sanitize).filter(|s| !s.is_empty());
        let title = title_opt.as_deref().map(sanitize).filter(|s| !s.is_empty());
        let desc = match (&email, &title) {
            (Some(e), Some(t)) => format!("{e} · {t}"),
            (Some(e), None) => e.clone(),
            (None, Some(t)) => t.clone(),
            (None, None) => format!("{sent} msgs"),
        };
        let val = match name {
            Some(n) if seen.insert(n.clone()) => n,
            _ => match email {
                Some(e) if seen.insert(e.clone()) => e,
                _ if seen.insert(mri.clone()) => mri,
                _ => continue,
            },
        };
        out.push((val, desc));
    }
    out
}

fn complete_tokens() -> Vec<(String, String)> {
    let Ok(res) = daemon::list_named_tokens(true) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for t in res["tokens"].as_array().into_iter().flatten() {
        let Some(name) = t["name"].as_str().map(sanitize).filter(|s| !s.is_empty()) else {
            continue;
        };
        let id = t["id"].as_str().unwrap_or("?");
        let state = if t["expired"].as_bool().unwrap_or(false) {
            "expired"
        } else {
            "active"
        };
        let exp = t["expiresAt"].as_str().unwrap_or("never");
        out.push((name, format!("id {id}, {state}, expires {exp}")));
    }
    out
}

fn complete_record_sources() -> Vec<(String, String)> {
    let Some(conn) = open_ro_fast() else {
        return Vec::new();
    };
    let Ok(mut stmt) = conn.prepare(
        "SELECT source, COUNT(DISTINCT record_key) AS records \
         FROM record_versions GROUP BY source ORDER BY source",
    ) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))) else {
        return Vec::new();
    };
    rows.flatten()
        .map(|(src, n)| (sanitize(&src), format!("{n} records")))
        .filter(|(s, _)| !s.is_empty())
        .collect()
}

fn complete_backups() -> Vec<(String, String)> {
    let mut bks = backups::list_backups(None);
    bks.reverse();
    bks.into_iter()
        .take(50)
        .map(|p| {
            let mb = p.metadata().map(|m| m.len() as f64 / 1e6).unwrap_or(0.0);
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            (p.to_string_lossy().into_owned(), format!("{name} ({mb:.1} MB)"))
        })
        .collect()
}

fn complete_file_types() -> Vec<(String, String)> {
    let fallback = || -> Vec<(String, String)> {
        [
            ("pdf", "PDF documents"),
            ("xlsx", "Excel spreadsheets"),
            ("docx", "Word documents"),
            ("pptx", "PowerPoint decks"),
            ("png", "PNG images"),
            ("jpg", "JPEG images"),
            ("zip", "ZIP archives"),
            ("csv", "CSV files"),
            ("txt", "Text files"),
            ("json", "JSON files"),
        ]
        .into_iter()
        .map(|(k, d)| (k.to_string(), d.to_string()))
        .collect()
    };
    let Some(conn) = open_ro_fast() else {
        return fallback();
    };
    let Ok(mut stmt) = conn.prepare("SELECT name FROM attachments WHERE kind = 'file' AND name IS NOT NULL") else {
        return fallback();
    };
    let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) else {
        return fallback();
    };
    let mut counts: HashMap<String, usize> = HashMap::new();
    for name in rows.flatten() {
        let trimmed = name.trim_matches(|c| c == '.' || c == ' ');
        if let Some((_, ext)) = trimmed.rsplit_once('.') {
            let ext = ext.trim().to_ascii_lowercase();
            if !ext.is_empty() && ext.len() <= 8 && ext.chars().all(|c| c.is_ascii_alphanumeric()) {
                *counts.entry(ext).or_insert(0) += 1;
            }
        }
    }
    if counts.is_empty() {
        return fallback();
    }
    let mut items: Vec<(String, usize)> = counts.into_iter().collect();
    items.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    items
        .into_iter()
        .take(30)
        .map(|(ext, n)| (ext, format!("{n} files")))
        .collect()
}

fn complete_times() -> Vec<(String, String)> {
    [
        ("today", "Start of today"),
        ("yesterday", "Start of yesterday"),
        ("1h", "Last hour"),
        ("6h", "Last 6 hours"),
        ("24h", "Last 24 hours"),
        ("7d", "Last 7 days"),
        ("14d", "Last 14 days"),
        ("30d", "Last 30 days"),
        ("90d", "Last 90 days"),
        ("all", "All time"),
    ]
    .into_iter()
    .map(|(k, d)| (k.to_string(), d.to_string()))
    .collect()
}

fn complete_expires() -> Vec<(String, String)> {
    [
        ("12h", "12 hours"),
        ("7d", "7 days"),
        ("30d", "30 days"),
        ("90d", "90 days"),
        ("1y", "1 year"),
        ("never", "No expiration"),
    ]
    .into_iter()
    .map(|(k, d)| (k.to_string(), d.to_string()))
    .collect()
}

fn complete_presence_durations() -> Vec<(String, String)> {
    [
        ("15m", "15 minutes"),
        ("30m", "30 minutes"),
        ("1h", "1 hour"),
        ("2h", "2 hours"),
        ("4h", "4 hours"),
        ("1d", "1 day"),
        ("never", "Until changed"),
    ]
    .into_iter()
    .map(|(k, d)| (k.to_string(), d.to_string()))
    .collect()
}

fn complete_scopes() -> Vec<(String, String)> {
    [
        ("read", "Read local archive endpoints"),
        ("sync", "Trigger archive sync"),
        ("presence:read", "Read live presence"),
        ("presence:write", "Set live availability and status message"),
        ("react", "React, pin, save, and mute"),
        ("mark-read", "Mark chats read/unread (sends read receipts)"),
        ("send", "Send and forward messages with media"),
        ("modify", "Edit and delete own messages"),
        ("chat:manage", "Create/rename chats and manage members"),
    ]
    .into_iter()
    .map(|(k, d)| (k.to_string(), d.to_string()))
    .collect()
}

fn complete_reactions() -> Vec<(String, String)> {
    [
        ("like", "Thumbs up"),
        ("heart", "Heart"),
        ("laugh", "Laugh"),
        ("surprised", "Surprised"),
        ("sad", "Sad"),
        ("angry", "Angry"),
        ("1f389_partypopper", "Party popper"),
        ("clappinghands", "Clapping hands"),
        ("praying", "Thanks / praying"),
        ("1f525_fire", "Fire"),
        ("launch", "Rocket"),
    ]
    .into_iter()
    .map(|(k, d)| (k.to_string(), d.to_string()))
    .collect()
}

fn complete_versions() -> Vec<(String, String)> {
    let mut out = Vec::new();
    let cur_tag = format!("v{}", crate::VERSION);
    if let Some(cache) = update::load_update_cache(&paths::update_check_path()) {
        if let Some(tag) = cache.tag.filter(|t| !t.is_empty() && t != &cur_tag) {
            out.push((tag, "Latest release".to_string()));
        }
    }
    out.push((cur_tag, "Current installed version".to_string()));
    out
}

fn arg_takes_value(arg: &Arg) -> bool {
    arg.get_action().takes_values()
}

fn arg_is_repeatable(arg: &Arg) -> bool {
    matches!(arg.get_action(), ArgAction::Append | ArgAction::Count)
}

fn find_long_arg<'a>(cmd: &'a Command, long_name: &str) -> Option<&'a Arg> {
    cmd.get_arguments()
        .find(|a| a.get_long() == Some(long_name) || a.get_all_aliases().is_some_and(|al| al.contains(&long_name)))
}

fn find_short_arg(cmd: &Command, ch: char) -> Option<&Arg> {
    cmd.get_arguments().find(|a| a.get_short() == Some(ch))
}

fn filter_for_shell(items: Vec<(String, String)>, current: &str, shell: &str) -> Vec<(String, String)> {
    if current.is_empty() || matches!(shell, "fish" | "zsh" | "elvish") {
        return items;
    }
    let needle = current.to_ascii_lowercase();
    let prefix: Vec<(String, String)> = items
        .iter()
        .filter(|(v, _)| v.to_ascii_lowercase().starts_with(&needle))
        .cloned()
        .collect();
    if !prefix.is_empty() {
        return prefix;
    }
    items
        .into_iter()
        .filter(|(v, _)| v.to_ascii_lowercase().contains(&needle))
        .collect()
}

fn complete_for_arg(path: &[String], arg: &Arg, current: &str, shell: &str) -> CompletionResult {
    let pvs = arg.get_possible_values();
    if !pvs.is_empty() {
        let items: Vec<(String, String)> = pvs
            .into_iter()
            .filter(|pv| !pv.is_hide_set())
            .map(|pv| {
                let desc = pv.get_help().map(|h| sanitize(&h.to_string())).unwrap_or_default();
                (pv.get_name().to_string(), desc)
            })
            .collect();
        return CompletionResult {
            items: filter_for_shell(items, current, shell),
            file_mode: FileMode::NoFiles,
        };
    }

    let long = arg.get_long();
    let id = arg.get_id().as_str();
    let p0 = path.first().map(String::as_str).unwrap_or("");
    let p1 = path.get(1).map(String::as_str).unwrap_or("");

    let (items, file_mode) = if matches!(long, Some("chat" | "chats" | "from-chat"))
        || (long.is_none() && (matches!(id, "chat" | "to_chat") || (p0 == "message" && p1 == "send" && id == "target")))
    {
        (complete_chats(), FileMode::NoFiles)
    } else if matches!(long, Some("from" | "member" | "with" | "mention"))
        || (long.is_none() && matches!(id, "person" | "member" | "members"))
    {
        (complete_people(), FileMode::NoFiles)
    } else if long == Some("scope") {
        (complete_scopes(), FileMode::NoFiles)
    } else if long.is_none() && id == "reaction" {
        (complete_reactions(), FileMode::NoFiles)
    } else if matches!(long, Some("since" | "until")) {
        (complete_times(), FileMode::NoFiles)
    } else if long == Some("expires") {
        (complete_expires(), FileMode::NoFiles)
    } else if long == Some("for") {
        (complete_presence_durations(), FileMode::NoFiles)
    } else if long == Some("type") && p0 == "file" {
        (complete_file_types(), FileMode::NoFiles)
    } else if long == Some("version") && p0 == "update" {
        (complete_versions(), FileMode::NoFiles)
    } else if long.is_none() && p0 == "token" && p1 == "revoke" && id == "target" {
        (complete_tokens(), FileMode::NoFiles)
    } else if long.is_none() && p0 == "debug" && p1 == "records" && id == "source" {
        (complete_record_sources(), FileMode::NoFiles)
    } else if long.is_none() && p0 == "archive" && matches!((p1, id), ("restore", "backup") | ("verify", "file")) {
        (complete_backups(), FileMode::Files)
    } else {
        match arg.get_value_hint() {
            ValueHint::DirPath => (Vec::new(), FileMode::Dirs),
            ValueHint::FilePath | ValueHint::AnyPath => (Vec::new(), FileMode::Files),
            _ => (Vec::new(), FileMode::NoFiles),
        }
    };

    if matches!(long, Some("scope" | "chats")) {
        if let Some((prefix, tail)) = current.rsplit_once(',') {
            let chosen: HashSet<String> = prefix
                .split(',')
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .collect();
            let remaining: Vec<(String, String)> = items
                .into_iter()
                .filter(|(v, _)| !chosen.contains(&v.to_ascii_lowercase()))
                .collect();
            let filtered = filter_for_shell(remaining, tail, shell);
            let prefixed: Vec<(String, String)> = filtered
                .into_iter()
                .map(|(v, d)| (format!("{prefix},{v}"), d))
                .collect();
            return CompletionResult {
                items: prefixed,
                file_mode,
            };
        }
    }

    CompletionResult {
        items: filter_for_shell(items, current, shell),
        file_mode,
    }
}

fn complete_flags(cmd: &Command, used_opts: &HashSet<String>, current: &str) -> CompletionResult {
    let mut items = Vec::new();
    let include_short = current == "-" || (current.starts_with('-') && !current.starts_with("--"));
    for arg in cmd.get_arguments() {
        if arg.is_positional() || arg.is_hide_set() {
            continue;
        }
        let key = arg.get_id().as_str();
        if !arg_is_repeatable(arg) && used_opts.contains(key) {
            continue;
        }
        let desc = arg.get_help().map(|h| sanitize(&h.to_string())).unwrap_or_default();
        if let Some(long) = arg.get_long() {
            let flag = format!("--{long}");
            if flag.starts_with(current) {
                items.push((flag, desc.clone()));
            }
        }
        if include_short {
            if let Some(short) = arg.get_short() {
                let flag = format!("-{short}");
                if flag.starts_with(current) {
                    items.push((flag, desc));
                }
            }
        }
    }
    if "--help".starts_with(current) {
        items.push(("--help".to_string(), "Print help".to_string()));
    }
    if include_short && "-h".starts_with(current) {
        items.push(("-h".to_string(), "Print help".to_string()));
    }
    if cmd.get_name() == "teams" {
        if "--version".starts_with(current) {
            items.push(("--version".to_string(), "Print version".to_string()));
        }
        if include_short && "-V".starts_with(current) {
            items.push(("-V".to_string(), "Print version".to_string()));
        }
    }
    CompletionResult {
        items,
        file_mode: FileMode::NoFiles,
    }
}

fn visible_subcommands(cmd: &Command) -> Vec<&Command> {
    cmd.get_subcommands()
        .filter(|s| !s.is_hide_set() && s.get_name() != "help")
        .collect()
}

pub fn complete_argv(root: &Command, words: &[String], shell: &str) -> CompletionResult {
    let (preceding, current) = match words.split_last() {
        Some((last, init)) => (init, last.as_str()),
        None => (&[][..], ""),
    };

    let mut cur_cmd = root;
    let mut path: Vec<String> = Vec::new();
    let mut pending_opt: Option<&Arg> = None;
    let mut used_opts: HashSet<String> = HashSet::new();
    let mut pos_count: usize = 0;
    let mut after_double_dash = false;

    for tok in preceding {
        if pending_opt.take().is_some() {
            continue;
        }
        if !after_double_dash && tok == "--" {
            after_double_dash = true;
            continue;
        }
        if !after_double_dash && tok.starts_with("--") {
            let rest = &tok[2..];
            if let Some((name, _)) = rest.split_once('=') {
                if let Some(arg) = find_long_arg(cur_cmd, name) {
                    used_opts.insert(arg.get_id().as_str().to_string());
                }
            } else if let Some(arg) = find_long_arg(cur_cmd, rest) {
                used_opts.insert(arg.get_id().as_str().to_string());
                if arg_takes_value(arg) {
                    pending_opt = Some(arg);
                }
            }
            continue;
        }
        if !after_double_dash && tok.starts_with('-') && tok.len() > 1 {
            if let Some(ch) = tok.chars().nth(1) {
                if let Some(arg) = find_short_arg(cur_cmd, ch) {
                    used_opts.insert(arg.get_id().as_str().to_string());
                    if arg_takes_value(arg) && tok.chars().count() == 2 {
                        pending_opt = Some(arg);
                    }
                }
            }
            continue;
        }
        if pos_count == 0 {
            if let Some(sub) = visible_subcommands(cur_cmd)
                .into_iter()
                .find(|s| s.get_name() == tok.as_str() || s.get_all_aliases().any(|a| a == tok.as_str()))
            {
                cur_cmd = sub;
                path.push(sub.get_name().to_string());
                used_opts.clear();
                continue;
            }
        }
        pos_count += 1;
    }

    if let Some(arg) = pending_opt {
        return complete_for_arg(&path, arg, current, shell);
    }

    if !after_double_dash && current.starts_with("--") {
        if let Some((flag_name, val_part)) = current[2..].split_once('=') {
            if let Some(arg) = find_long_arg(cur_cmd, flag_name) {
                if arg_takes_value(arg) {
                    let mut res = complete_for_arg(&path, arg, val_part, shell);
                    let pfx = format!("--{flag_name}=");
                    for (v, _) in &mut res.items {
                        *v = format!("{pfx}{v}");
                    }
                    return res;
                }
            }
            return CompletionResult {
                items: Vec::new(),
                file_mode: FileMode::NoFiles,
            };
        }
    }

    if !after_double_dash && current.starts_with('-') {
        return complete_flags(cur_cmd, &used_opts, current);
    }

    let subs = visible_subcommands(cur_cmd);
    if !subs.is_empty() && pos_count == 0 {
        let items: Vec<(String, String)> = subs
            .into_iter()
            .map(|s| {
                let desc = s.get_about().map(|a| sanitize(&a.to_string())).unwrap_or_default();
                (s.get_name().to_string(), desc)
            })
            .collect();
        return CompletionResult {
            items: filter_for_shell(items, current, shell),
            file_mode: FileMode::NoFiles,
        };
    }

    let positionals: Vec<&Arg> = cur_cmd
        .get_arguments()
        .filter(|a| a.is_positional() && !a.is_hide_set())
        .collect();
    let pos_arg = positionals.get(pos_count).copied().or_else(|| {
        positionals
            .last()
            .copied()
            .filter(|a| arg_is_repeatable(a) || a.get_num_args().is_some_and(|r| r.max_values() > 1))
    });
    if let Some(arg) = pos_arg {
        return complete_for_arg(&path, arg, current, shell);
    }

    CompletionResult {
        items: Vec::new(),
        file_mode: FileMode::NoFiles,
    }
}

pub fn print_complete(root: &Command, words: &[String], shell: &str) {
    let res = complete_argv(root, words, shell);
    let include_desc = shell != "bash";
    for (val, desc) in res.items {
        if include_desc && !desc.is_empty() {
            println!("{val}\t{desc}");
        } else {
            println!("{val}");
        }
    }
    println!("{}", res.file_mode.directive());
}

const FISH_SCRIPT: &str = r#"complete -c teams -e

function __fish_teams_complete
    set -l tokens (commandline -opc)
    set -e tokens[1]
    set -l cur (commandline -ct)
    set -l lines (teams __complete --shell fish -- $tokens $cur 2>/dev/null)
    if test (count $lines) -eq 0
        return
    end
    set -l directive $lines[-1]
    set -e lines[-1]
    for line in $lines
        if test -n "$line"
            printf '%s\n' $line
        end
    end
    if test "$directive" = ":files"
        __fish_complete_path "$cur"
    else if test "$directive" = ":dirs"
        __fish_complete_directories "$cur"
    end
end

complete -c teams -k -f -a "(__fish_teams_complete)"
"#;

const ZSH_SCRIPT: &str = r#"#compdef teams

_teams() {
    local -a raw items
    local line directive val desc cur
    cur="${words[CURRENT]}"
    raw=("${(@f)$(teams __complete --shell zsh -- "${(@)words[2,CURRENT-1]}" "$cur" 2>/dev/null)}")
    if (( ${#raw} == 0 )); then
        return 0
    fi
    directive="${raw[-1]}"
    raw=("${(@)raw[1,-2]}")
    items=()
    for line in "${raw[@]}"; do
        [[ -z "$line" ]] && continue
        if [[ "$line" == *$'\t'* ]]; then
            val="${line%%$'\t'*}"
            desc="${line#*$'\t'}"
            items+=("${val//:/\\:}:${desc}")
        else
            items+=("${line//:/\\:}")
        fi
    done
    if (( ${#items} > 0 )); then
        _describe -V 'teams' items
    fi
    if [[ "$directive" == ":files" ]]; then
        _files
    elif [[ "$directive" == ":dirs" ]]; then
        _files -/
    fi
}

if [[ "${funcstack[1]}" == "_teams" ]]; then
    _teams "$@"
else
    compdef _teams teams
fi
"#;

const BASH_SCRIPT: &str = r#"_teams_complete() {
    local cur line directive quoted
    local -a raw args
    COMPREPLY=()
    cur="${COMP_WORDS[COMP_CWORD]}"
    args=("${COMP_WORDS[@]:1:COMP_CWORD}")
    raw=()
    while IFS= read -r line; do
        raw+=("$line")
    done < <(teams __complete --shell bash -- "${args[@]}" 2>/dev/null)
    if [[ ${#raw[@]} -eq 0 ]]; then
        return 0
    fi
    directive="${raw[${#raw[@]}-1]}"
    unset 'raw[${#raw[@]}-1]'
    for line in "${raw[@]}"; do
        [[ -z "$line" ]] && continue
        if [[ "$cur" == \'* || "$cur" == \"* ]]; then
            COMPREPLY+=("$line")
        else
            printf -v quoted '%q' "$line"
            COMPREPLY+=("$quoted")
        fi
    done
    if [[ "$directive" == ":files" ]]; then
        while IFS= read -r line; do
            COMPREPLY+=("$line")
        done < <(compgen -f -- "$cur")
    elif [[ "$directive" == ":dirs" ]]; then
        while IFS= read -r line; do
            COMPREPLY+=("$line")
        done < <(compgen -d -- "$cur")
    fi
}

complete -F _teams_complete teams
"#;

const ELVISH_SCRIPT: &str = r#"use str

set edit:completion:arg-completer[teams] = {|@words|
    var args = $words[1..]
    var lines = [(try { teams __complete --shell elvish -- $@args 2>/dev/null } catch { })]
    if (== (count $lines) 0) {
        return
    }
    var directive = $lines[-1]
    for line $lines[0..-1] {
        if (eq $line '') {
            continue
        }
        var parts = [(str:split "\t" &max=2 $line)]
        if (== (count $parts) 2) {
            edit:complex-candidate $parts[0] &display=$parts[0]'  ('$parts[1]')'
        } else {
            edit:complex-candidate $parts[0]
        }
    }
    if (or (eq $directive ':files') (eq $directive ':dirs')) {
        edit:complete-filename $words[-1]
    }
}
"#;

const POWERSHELL_SCRIPT: &str = r#"using namespace System.Management.Automation

Register-ArgumentCompleter -Native -CommandName 'teams' -ScriptBlock {
    param($wordToComplete, $commandAst, $cursorPosition)
    $tokens = @()
    $elems = $commandAst.CommandElements
    for ($i = 1; $i -lt $elems.Count; $i++) {
        $text = $elems[$i].Extent.Text
        if ($text -eq $wordToComplete -and $i -eq ($elems.Count - 1)) {
            continue
        }
        $tokens += $text
    }
    $tokens += $wordToComplete
    $lines = @(& teams __complete --shell powershell -- @tokens 2>$null)
    if ($lines.Count -eq 0) { return }
    $directive = $lines[-1]
    for ($i = 0; $i -lt ($lines.Count - 1); $i++) {
        $line = $lines[$i]
        if ([string]::IsNullOrEmpty($line)) { continue }
        $parts = $line -split "`t", 2
        $val = $parts[0]
        $desc = if ($parts.Count -gt 1) { $parts[1] } else { $val }
        $escaped = if ($val -match '\s') { "'$($val -replace "'", "''")'" } else { $val }
        [CompletionResult]::new($escaped, $val, [CompletionResultType]::ParameterValue, $desc)
    }
    if ($directive -eq ':files' -or $directive -eq ':dirs') {
        Get-ChildItem -Path "$wordToComplete*" -ErrorAction SilentlyContinue | ForEach-Object {
            if ($directive -eq ':files' -or $_.PSIsContainer) {
                [CompletionResult]::new($_.FullName, $_.Name, [CompletionResultType]::ProviderItem, $_.FullName)
            }
        }
    }
}
"#;

pub fn generate_script(shell: Shell) -> &'static str {
    match shell {
        Shell::Fish => FISH_SCRIPT,
        Shell::Zsh => ZSH_SCRIPT,
        Shell::Bash => BASH_SCRIPT,
        Shell::Elvish => ELVISH_SCRIPT,
        Shell::PowerShell => POWERSHELL_SCRIPT,
        _ => "",
    }
}
