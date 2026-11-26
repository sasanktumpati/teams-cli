use crate::formatters::Quoted;
use crate::live::{cookies, local_onedrive, transport};
use crate::paths;
use crate::storage::db;
use regex::Regex;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use url::Url;

const MAX_REDIRECTS: usize = 10;
const CHUNK_SIZE: usize = 65_536;
pub const DEFAULT_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub const MAX_FILENAME_BYTES: usize = 200;

static SHAREPOINT_SHARING_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"/:\w:/").unwrap());
static RFC5987_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)filename\*\s*=\s*(?:UTF-8)?'[^']*'([^;\r\n]+)").unwrap());
static CD_FILENAME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)filename\s*=\s*(?:"([^"]+)"|([^;\r\n]+))"#).unwrap());

#[derive(Debug, Clone)]
pub struct FetchError(pub String);

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for FetchError {}

#[derive(Debug, Clone)]
pub struct ResolvedAttachment {
    pub url: String,
    pub name: Option<String>,
    pub kind: String,
    pub message_id: Option<String>,
    #[allow(dead_code)]
    pub chat_id: Option<String>,
}

pub fn downloads_dir() -> PathBuf {
    let d = paths::home().join("downloads");
    let _ = paths::private_dir(d.clone());
    d
}

pub fn rewrite_download_url(url_str: &str) -> String {
    let Ok(mut u) = Url::parse(url_str) else {
        return url_str.to_string();
    };
    let host = u.host_str().unwrap_or("").trim().to_ascii_lowercase();
    let host = host.trim_end_matches('.');
    if !(host == "sharepoint.com" || host.ends_with(".sharepoint.com")) {
        return url_str.to_string();
    }
    if !SHAREPOINT_SHARING_RE.is_match(u.path()) {
        return url_str.to_string();
    }
    if u.query_pairs().any(|(k, _)| k.eq_ignore_ascii_case("download")) {
        return url_str.to_string();
    }
    let new_q = match u.query() {
        Some(q) if !q.is_empty() => format!("{q}&download=1"),
        _ => "download=1".to_string(),
    };
    u.set_query(Some(&new_q));
    u.to_string()
}

fn direct_url_attachment(url_str: &str) -> ResolvedAttachment {
    let host = Url::parse(url_str)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
        .unwrap_or_default();
    let kind = if host == "asm.skype.com" || host.ends_with(".asm.skype.com") {
        "image"
    } else {
        "file"
    };
    ResolvedAttachment {
        url: url_str.to_string(),
        name: None,
        kind: kind.to_string(),
        message_id: None,
        chat_id: None,
    }
}

fn select_from_rows(
    rows: Vec<ResolvedAttachment>,
    label: &str,
    index: Option<usize>,
) -> Result<ResolvedAttachment, FetchError> {
    if rows.is_empty() {
        return Err(FetchError(format!(
            "No attachments found for message {}.",
            Quoted(label)
        )));
    }
    if let Some(idx) = index {
        if idx < 1 || idx > rows.len() {
            return Err(FetchError(format!(
                "Attachment index {idx} out of range (message {} has {} attachment(s)).",
                Quoted(label),
                rows.len()
            )));
        }
        return Ok(rows[idx - 1].clone());
    }
    if rows.len() == 1 {
        return Ok(rows[0].clone());
    }
    let mut lines = vec![format!(
        "Message {} has {} attachments; specify --index (1..{}):",
        Quoted(label),
        rows.len(),
        rows.len()
    )];
    for (i, r) in rows.iter().enumerate() {
        let name = r.name.as_deref().filter(|s| !s.is_empty()).unwrap_or("(unnamed)");
        lines.push(format!("  {}. [{}] {name} — {}", i + 1, r.kind, r.url));
    }
    Err(FetchError(lines.join("\n")))
}

fn query_att_rows(
    conn: &Connection,
    sql: &str,
    params: &[&dyn rusqlite::ToSql],
) -> Result<Vec<ResolvedAttachment>, FetchError> {
    let mut stmt = conn.prepare(sql).map_err(|e| FetchError(e.to_string()))?;
    let rows = stmt
        .query_map(params, |r| {
            Ok(ResolvedAttachment {
                chat_id: r.get(1)?,
                message_id: r.get(2)?,
                kind: r.get(3)?,
                name: r.get(4)?,
                url: r.get(5)?,
            })
        })
        .map_err(|e| FetchError(e.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| FetchError(e.to_string()))?;
    Ok(rows)
}

pub fn resolve_attachment(
    conn: &Connection,
    target: Option<&str>,
    message_id: Option<&str>,
    index: Option<usize>,
) -> Result<ResolvedAttachment, FetchError> {
    if let Some(0) = index {
        return Err(FetchError("Attachment index must be >= 1.".to_string()));
    }
    let target_s = target.unwrap_or("").trim();
    let msg_id_s = message_id.unwrap_or("").trim();
    if target_s.is_empty() && msg_id_s.is_empty() {
        return Err(FetchError(
            "Provide a target URL, filename/substring, or message_id.".to_string(),
        ));
    }

    let lower = target_s.to_ascii_lowercase();
    if lower.starts_with("https://") || lower.starts_with("http://") {
        let rows = query_att_rows(
            conn,
            "SELECT a.id, a.conversation_id, a.message_id, a.kind, a.name, a.url \
             FROM attachments a LEFT JOIN messages m ON m.conversation_id = a.conversation_id AND m.message_id = a.message_id \
             WHERE a.url = ? ORDER BY COALESCE(m.arrived_at, 0) DESC, a.id DESC LIMIT 1",
            &[&target_s],
        )?;
        return Ok(rows
            .into_iter()
            .next()
            .unwrap_or_else(|| direct_url_attachment(target_s)));
    }

    if !msg_id_s.is_empty() {
        let rows = if let Some((cid, mid)) = msg_id_s.rsplit_once('/') {
            query_att_rows(
                conn,
                "SELECT id, conversation_id, message_id, kind, name, url FROM attachments WHERE conversation_id = ? AND message_id = ? ORDER BY id",
                &[&cid, &mid],
            )?
        } else {
            query_att_rows(
                conn,
                "SELECT id, conversation_id, message_id, kind, name, url FROM attachments WHERE message_id = ? ORDER BY id",
                &[&msg_id_s],
            )?
        };
        return select_from_rows(rows, msg_id_s, index);
    }

    if let Some((cid, mid)) = target_s.rsplit_once('/') {
        if !cid.is_empty() && !mid.is_empty() {
            let rows = query_att_rows(
                conn,
                "SELECT id, conversation_id, message_id, kind, name, url FROM attachments WHERE conversation_id = ? AND message_id = ? ORDER BY id",
                &[&cid, &mid],
            )?;
            if !rows.is_empty() {
                return select_from_rows(rows, target_s, index);
            }
            let has_msg = conn
                .query_row(
                    "SELECT 1 FROM messages WHERE conversation_id = ? AND message_id = ? LIMIT 1",
                    params![cid, mid],
                    |_| Ok(()),
                )
                .optional()
                .ok()
                .flatten()
                .is_some();
            if has_msg {
                return Err(FetchError(format!(
                    "No attachments found for message {}.",
                    Quoted(target_s)
                )));
            }
        }
    }

    if target_s.chars().all(|c| c.is_ascii_digit()) {
        let rows = query_att_rows(
            conn,
            "SELECT id, conversation_id, message_id, kind, name, url FROM attachments WHERE message_id = ? ORDER BY id",
            &[&target_s],
        )?;
        if !rows.is_empty() {
            return select_from_rows(rows, target_s, index);
        }
    }

    let pat = db::like_pattern(target_s);
    let rows = query_att_rows(
        conn,
        "SELECT a.id, a.conversation_id, a.message_id, a.kind, a.name, a.url \
         FROM attachments a JOIN messages m ON m.conversation_id = a.conversation_id AND m.message_id = a.message_id \
         WHERE a.name LIKE ? ESCAPE '\\' OR a.url LIKE ? ESCAPE '\\' \
         ORDER BY COALESCE(m.arrived_at, 0) DESC, a.id DESC",
        &[&pat, &pat],
    )?;
    if rows.is_empty() {
        return Err(FetchError(format!("No attachment matches {}.", Quoted(target_s))));
    }

    let mut distinct = Vec::new();
    let mut seen_urls = HashSet::new();
    for r in rows {
        if seen_urls.insert(r.url.clone()) {
            distinct.push(r);
        }
    }
    if let Some(idx) = index {
        if idx > distinct.len() {
            return Err(FetchError(format!(
                "Attachment index {idx} out of range ({} distinct match(es) for {}).",
                distinct.len(),
                Quoted(target_s)
            )));
        }
        return Ok(distinct[idx - 1].clone());
    }
    if distinct.len() == 1 {
        return Ok(distinct[0].clone());
    }
    let q_lower = target_s.to_ascii_lowercase();
    if q_lower != "image" {
        if let Some(exact) = distinct
            .iter()
            .find(|r| r.name.as_deref().unwrap_or("").trim().to_ascii_lowercase() == q_lower)
        {
            return Ok(exact.clone());
        }
    }
    let mut lines = vec![format!(
        "Ambiguous target {} matches {} attachments; specify a more specific name, --message, URL, or --index:",
        Quoted(target_s),
        distinct.len()
    )];
    for (i, r) in distinct.iter().take(10).enumerate() {
        let name = r.name.as_deref().filter(|s| !s.is_empty()).unwrap_or("(unnamed)");
        let mid = r.message_id.as_deref().unwrap_or("?");
        lines.push(format!("  {}. [{}] {name} (message {mid}) — {}", i + 1, r.kind, r.url));
    }
    Err(FetchError(lines.join("\n")))
}

fn extension_for_content_type(ct: &str) -> &'static str {
    let mime = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    match mime.as_str() {
        "image/jpeg" | "image/jpg" => ".jpg",
        "image/png" => ".png",
        "image/gif" => ".gif",
        "image/webp" => ".webp",
        "image/svg+xml" => ".svg",
        "image/bmp" => ".bmp",
        "image/tiff" => ".tiff",
        "image/heic" => ".heic",
        "application/pdf" => ".pdf",
        "text/plain" => ".txt",
        "text/csv" => ".csv",
        "text/html" => ".html",
        "application/json" => ".json",
        "application/zip" => ".zip",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => ".docx",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => ".xlsx",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => ".pptx",
        _ => "",
    }
}

fn parse_cd_filename(header: Option<&str>) -> Option<String> {
    let h = header?;
    if let Some(cap) = RFC5987_RE.captures(h) {
        let v = cookies::urlencoding_decode(cap[1].trim().trim_matches('"'));
        if !v.is_empty() {
            return Some(v);
        }
    }
    if let Some(cap) = CD_FILENAME_RE.captures(h) {
        let v = cap
            .get(1)
            .or_else(|| cap.get(2))
            .map(|m| m.as_str().trim().to_string())?;
        if !v.is_empty() {
            return Some(v);
        }
    }
    None
}

fn url_basename_candidate(url_str: &str) -> Option<String> {
    let u = Url::parse(url_str).ok()?;
    if SHAREPOINT_SHARING_RE.is_match(u.path()) {
        return None;
    }
    let seg = cookies::urlencoding_decode(u.path().trim_end_matches('/').rsplit('/').next()?);
    if seg.is_empty() || matches!(seg.as_str(), "imgo" | "imgpsh_fullsize_anim" | "download.aspx") || !seg.contains('.')
    {
        return None;
    }
    Some(seg)
}

pub fn sanitize_filename(name: &str) -> String {
    let base = name.replace('\\', "/");
    let base = base.rsplit('/').next().unwrap_or("");
    let cleaned: String = base
        .chars()
        .filter(|&c| (c as u32) >= 0x20 && (c as u32) != 0x7f)
        .map(|c| if c == ':' { '_' } else { c })
        .collect();
    let trimmed = cleaned.trim_matches(|c| c == ' ' || c == '.');
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        return "download".to_string();
    }
    if trimmed.len() > MAX_FILENAME_BYTES {
        if let Some((stem, ext)) = trimmed.rsplit_once('.') {
            if ext.len() <= 16 {
                let keep = MAX_FILENAME_BYTES.saturating_sub(ext.len() + 1);
                return format!("{}.{ext}", truncate_utf8(stem, keep));
            }
        }
        return truncate_utf8(trimmed, MAX_FILENAME_BYTES).to_string();
    }
    trimmed.to_string()
}

fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn choose_filename(att: &ResolvedAttachment, cd: Option<&str>, final_url: &str, ct: &str) -> String {
    let ext = extension_for_content_type(ct);
    let mut raw = att.name.as_deref().unwrap_or("").trim().to_string();
    let rl = raw.to_ascii_lowercase();
    if (att.kind == "image" && rl == "image")
        || raw == att.url
        || rl.starts_with("http://")
        || rl.starts_with("https://")
    {
        raw.clear();
    }
    if raw.is_empty() {
        raw = parse_cd_filename(cd).unwrap_or_default();
    }
    if raw.is_empty() {
        raw = url_basename_candidate(final_url)
            .or_else(|| url_basename_candidate(&att.url))
            .unwrap_or_default();
    }
    if !raw.is_empty() {
        let mut clean = sanitize_filename(&raw);
        if !clean.contains('.') && !ext.is_empty() && (att.kind == "image" || ct.starts_with("image/")) {
            clean.push_str(ext);
        }
        return clean;
    }
    let tag = att.message_id.clone().unwrap_or_else(|| {
        let d = Sha256::digest(att.url.as_bytes());
        hex::encode(&d[..5])
    });
    let pfx = if att.kind == "image" || ct.starts_with("image/") {
        "image"
    } else {
        "attachment"
    };
    sanitize_filename(&format!("{pfx}-{tag}{ext}"))
}

fn looks_like_login_html(sample: &[u8]) -> bool {
    let lower = String::from_utf8_lossy(sample).to_ascii_lowercase();
    [
        "login.microsoftonline.com",
        "sign in to your account",
        "sign in to your microsoft account",
        "aadsts",
        "login.live.com",
    ]
    .iter()
    .any(|m| lower.contains(m))
}

fn suffixed_filename(stem: &str, suffix: &str, ext: &str) -> String {
    let max_stem = MAX_FILENAME_BYTES.saturating_sub(suffix.len() + ext.len()).max(1);
    let mut prefix = truncate_utf8(stem, max_stem);
    if prefix.is_empty() {
        prefix = stem.chars().next().map_or("", |c| &stem[..c.len_utf8()]);
    }
    sanitize_filename(&format!("{prefix}{suffix}{ext}"))
}

pub fn disambiguate_dest(dest: PathBuf, msg_id: Option<&str>) -> PathBuf {
    if !dest.exists() && !dest.is_symlink() {
        return dest;
    }
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    let fname = dest.file_name().and_then(|n| n.to_str()).unwrap_or("download");
    let (stem, ext) = match fname.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (fname.to_string(), String::new()),
    };
    let mut base_stem = stem.clone();
    if let Some(mid) = msg_id {
        let sm = sanitize_filename(mid);
        if !sm.is_empty() && !stem.ends_with(&format!("-{sm}")) {
            let cand = parent.join(suffixed_filename(&stem, &format!("-{sm}"), &ext));
            if !cand.exists() && !cand.is_symlink() {
                return cand;
            }
            base_stem = format!("{stem}-{sm}");
        }
    }
    for n in 2..100_000 {
        let cand = parent.join(suffixed_filename(&base_stem, &format!("-{n}"), &ext));
        if !cand.exists() && !cand.is_symlink() {
            return cand;
        }
    }
    dest
}

fn read_up_to<R: Read>(reader: &mut R, limit: usize) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(limit.min(CHUNK_SIZE));
    let mut chunk = [0u8; 8192];
    while buf.len() < limit {
        let want = (limit - buf.len()).min(chunk.len());
        let n = reader.read(&mut chunk[..want])?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Ok(buf)
}

const FETCH_DISK_MARGIN_BYTES: u64 = 64 * 1024 * 1024;
const STREAM_DISK_CHECK_STEP: u64 = 8 * 1024 * 1024;

fn check_disk_headroom(dir: &Path, needed_bytes: u64) -> Result<(), FetchError> {
    let free = paths::disk_free_bytes(dir);
    if free > 0 && free < needed_bytes.saturating_add(FETCH_DISK_MARGIN_BYTES) {
        return Err(FetchError(format!(
            "Not enough free disk space ({:.1} MB free) for this download.",
            free as f64 / 1e6
        )));
    }
    Ok(())
}

fn resolve_dest(out: Option<&str>, filename: &str, msg_id: Option<&str>) -> PathBuf {
    let Some(out_s) = out else {
        return disambiguate_dest(downloads_dir().join(filename), msg_id);
    };
    let p = paths::expanduser(out_s);
    if p.is_dir() || out_s.ends_with('/') {
        let _ = fs::create_dir_all(&p);
        return disambiguate_dest(p.join(filename), msg_id);
    }
    if let Some(parent) = p.parent() {
        let _ = fs::create_dir_all(parent);
    }
    p
}

fn guess_content_type(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "txt" => "text/plain",
        "csv" => "text/csv",
        "json" => "application/json",
        "zip" => "application/zip",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        _ => "application/octet-stream",
    }
}

pub fn fetch_attachment(
    conn: Option<&Connection>,
    target: Option<&str>,
    message_id: Option<&str>,
    index: Option<usize>,
    out: Option<&str>,
    force_download: bool,
) -> Result<Value, FetchError> {
    fetch_attachment_with_limit(conn, target, message_id, index, out, force_download, DEFAULT_MAX_BYTES)
}

#[allow(clippy::too_many_arguments)]
pub fn fetch_attachment_with_limit(
    conn: Option<&Connection>,
    target: Option<&str>,
    message_id: Option<&str>,
    index: Option<usize>,
    out: Option<&str>,
    force_download: bool,
    max_bytes: u64,
) -> Result<Value, FetchError> {
    let att = match conn {
        None => {
            let ts = target.unwrap_or("").trim();
            let tl = ts.to_ascii_lowercase();
            if !tl.starts_with("https://") && !tl.starts_with("http://") {
                return Err(FetchError(
                    "A direct http(s) URL target is required when no archive connection is provided.".to_string(),
                ));
            }
            direct_url_attachment(ts)
        }
        Some(c) => resolve_attachment(c, target, message_id, index)?,
    };

    if att.kind == "file" && !force_download {
        let lookup = att.name.clone().or_else(|| url_basename_candidate(&att.url));
        if let Some(lname) = lookup {
            if let Some(od_path) = local_onedrive::find_onedrive_file(&lname, None) {
                let fname = od_path.file_name().unwrap().to_string_lossy().into_owned();
                let ct = guess_content_type(&fname);
                if out.is_none() {
                    let sz = od_path.metadata().map(|m| m.len()).unwrap_or(0);
                    return Ok(json!({
                        "path": od_path.to_string_lossy(),
                        "sizeBytes": sz,
                        "contentType": ct,
                        "url": att.url,
                        "name": fname,
                        "kind": att.kind,
                        "source": "onedrive",
                    }));
                }
                let clean = sanitize_filename(&fname);
                let src_len = od_path.metadata().map(|m| m.len()).unwrap_or(0);
                if max_bytes > 0 && src_len > max_bytes {
                    return Err(FetchError(format!(
                        "Refusing copy of {src_len} bytes (exceeds limit of {max_bytes} bytes; pass --max-bytes to override)."
                    )));
                }
                let dest = resolve_dest(out, &clean, att.message_id.as_deref());
                let parent = dest.parent().unwrap_or_else(|| Path::new("."));
                check_disk_headroom(parent, src_len)?;
                let sz = paths::atomic_copy_file(&od_path, &dest, 0o600, ".teams-fetch-")
                    .map_err(|e| FetchError(e.to_string()))?;
                return Ok(json!({
                    "path": dest.to_string_lossy(),
                    "sizeBytes": sz,
                    "contentType": ct,
                    "url": att.url,
                    "name": clean,
                    "kind": att.kind,
                    "source": "onedrive",
                }));
            }
        }
    }

    let cookies = if cookies::is_trusted_url(&att.url) {
        cookies::load_cookies(None, None, None).map_err(|e| FetchError(e.to_string()))?
    } else {
        Vec::new()
    };

    let agent = transport::agent(transport::READ_TIMEOUT);

    let mut current_url = rewrite_download_url(&att.url);
    let mut final_resp = None;
    for hop in 0..=MAX_REDIRECTS {
        let parsed =
            Url::parse(&current_url).map_err(|e| FetchError(format!("Invalid URL {}: {e}", Quoted(&current_url))))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(FetchError(format!(
                "Unsupported URL scheme: {}",
                Quoted(parsed.scheme())
            )));
        }
        if cookies::is_login_host(parsed.host_str()) {
            return Err(FetchError(
                "Teams session cookies have expired (redirected to Microsoft login). \
                 Open Microsoft Teams to refresh your session, then try again."
                    .to_string(),
            ));
        }
        let mut req = agent.get(&current_url).set("User-Agent", cookies::USER_AGENT);
        let hdr = cookies::build_cookie_header(&cookies, &current_url, None);
        if !hdr.is_empty() {
            req = req.set("Cookie", &hdr);
        }
        let resp = match req.call() {
            Ok(r) => r,
            Err(ureq::Error::Status(code, r)) => {
                if matches!(code, 401 | 403) {
                    return Err(FetchError(format!(
                        "Authentication failed (HTTP {code}). \
                         Open Microsoft Teams to refresh your session cookies, then try again."
                    )));
                }
                if !matches!(code, 301 | 302 | 303 | 307 | 308) {
                    return Err(FetchError(format!("Download failed with HTTP {code}")));
                }
                r
            }
            Err(ureq::Error::Transport(t)) => return Err(FetchError(format!("Download failed: {t}"))),
        };
        let status = resp.status();
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            let loc = resp
                .header("Location")
                .ok_or_else(|| FetchError(format!("HTTP {status} redirect without Location header")))?;
            if hop >= MAX_REDIRECTS {
                return Err(FetchError(format!(
                    "Too many redirects (exceeded {MAX_REDIRECTS} hops)"
                )));
            }
            current_url = parsed
                .join(loc)
                .map_err(|e| FetchError(format!("Invalid redirect URL: {e}")))?
                .to_string();
            continue;
        }
        final_resp = Some(resp);
        break;
    }

    let resp = final_resp.ok_or_else(|| FetchError(format!("Too many redirects (exceeded {MAX_REDIRECTS} hops)")))?;
    let raw_ct = resp.header("Content-Type").unwrap_or("application/octet-stream");
    let content_type = raw_ct
        .split(';')
        .next()
        .unwrap_or("application/octet-stream")
        .trim()
        .to_ascii_lowercase();
    let content_disp = resp.header("Content-Disposition").map(String::from);
    let content_len = resp.header("Content-Length").and_then(|s| s.trim().parse::<u64>().ok());
    if max_bytes > 0 {
        if let Some(cl) = content_len {
            if cl > max_bytes {
                return Err(FetchError(format!(
                    "Refusing download of {cl} bytes (exceeds limit of {max_bytes} bytes; pass --max-bytes to override)."
                )));
            }
        }
    }

    let mut reader = resp.into_reader();
    let first = read_up_to(&mut reader, CHUNK_SIZE).map_err(|e| FetchError(e.to_string()))?;

    let exp_html = att
        .name
        .as_deref()
        .map(|n| n.to_ascii_lowercase())
        .map(|n| n.ends_with(".html") || n.ends_with(".htm"))
        .unwrap_or(false);
    if content_type == "text/html" && !exp_html && looks_like_login_html(&first) {
        return Err(FetchError(
            "Received a Microsoft login page instead of the file. \
             Open Microsoft Teams to refresh your session cookies, then try again."
                .to_string(),
        ));
    }

    let filename = choose_filename(&att, content_disp.as_deref(), &current_url, &content_type);
    let dest = resolve_dest(out, &filename, att.message_id.as_deref());
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    check_disk_headroom(parent, content_len.unwrap_or(first.len() as u64))?;
    let mut rnd = [0u8; 6];
    getrandom::getrandom(&mut rnd).map_err(|e| FetchError(format!("Could not generate a temporary file name: {e}")))?;
    let tmp = parent.join(format!(
        ".teams-fetch-{}.{}.partial",
        std::process::id(),
        hex::encode(rnd)
    ));

    let size_bytes = (|| -> Result<u64, FetchError> {
        let mut fout = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| FetchError(e.to_string()))?;
        let mut total = 0u64;
        if !first.is_empty() {
            total += first.len() as u64;
            if max_bytes > 0 && total > max_bytes {
                return Err(FetchError(format!(
                    "Download exceeded limit of {max_bytes} bytes (pass --max-bytes to override)."
                )));
            }
            fout.write_all(&first).map_err(|e| FetchError(e.to_string()))?;
        }
        let mut buf = vec![0u8; CHUNK_SIZE];
        let mut next_disk_check = STREAM_DISK_CHECK_STEP;
        loop {
            let n = reader.read(&mut buf).map_err(|e| FetchError(e.to_string()))?;
            if n == 0 {
                break;
            }
            total += n as u64;
            if max_bytes > 0 && total > max_bytes {
                return Err(FetchError(format!(
                    "Download exceeded limit of {max_bytes} bytes (pass --max-bytes to override)."
                )));
            }
            if content_len.is_none() && total >= next_disk_check {
                check_disk_headroom(parent, STREAM_DISK_CHECK_STEP)?;
                next_disk_check = total.saturating_add(STREAM_DISK_CHECK_STEP);
            }
            fout.write_all(&buf[..n]).map_err(|e| FetchError(e.to_string()))?;
        }
        fout.sync_all().map_err(|e| FetchError(e.to_string()))?;
        drop(fout);
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
        fs::rename(&tmp, &dest).map_err(|e| FetchError(e.to_string()))?;
        paths::fsync_dir(parent);
        Ok(total)
    })();

    if size_bytes.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    let sz = size_bytes?;
    Ok(json!({
        "path": dest.to_string_lossy(),
        "sizeBytes": sz,
        "contentType": content_type,
        "url": att.url,
        "name": filename,
        "kind": att.kind,
        "source": "download",
    }))
}
