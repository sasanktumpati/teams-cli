use crate::live::error::LiveError;
use crate::live::refs::UploadedFile;
use crate::live::session::SharePointSession;
use crate::live::transport::{Transport, WRITE_TIMEOUT};
use serde_json::{json, Value};

pub const MAX_SIMPLE_FILE_BYTES: usize = 4 * 1024 * 1024;

fn encode_odata_string(s: &str) -> String {
    let escaped = s.replace('\'', "''");
    let mut out = String::with_capacity(escaped.len());
    for b in escaped.bytes() {
        let safe = b.is_ascii_alphanumeric() || matches!(b, b'/' | b'_' | b'-' | b'.' | b'~');
        if safe {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn personal_segment_from_email(email: &str) -> String {
    email
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

fn fetch_digest(transport: &Transport, site_url: &str, cookie_header: &str) -> Result<String, LiveError> {
    let url = format!("{}/_api/contextinfo", site_url.trim_end_matches('/'));
    let resp = transport.request_bytes(
        "POST",
        &url,
        &[
            ("Cookie", cookie_header),
            ("Accept", "application/json;odata=nometadata"),
        ],
        Some(b""),
        &[cookie_header],
        WRITE_TIMEOUT,
    )?;
    resp.json()
        .and_then(|v| v.get("FormDigestValue").and_then(|s| s.as_str()).map(String::from))
        .filter(|s| !s.is_empty())
        .ok_or_else(|| LiveError::upstream("SharePoint did not return FormDigestValue."))
}

fn resolve_personal_site_and_folder(
    transport: &Transport,
    base_url: &str,
    cookie_header: &str,
) -> Result<(String, String), LiveError> {
    let url = format!("{}/_api/web/currentuser", base_url.trim_end_matches('/'));
    let resp = transport.request_bytes(
        "GET",
        &url,
        &[
            ("Cookie", cookie_header),
            ("Accept", "application/json;odata=nometadata"),
        ],
        None,
        &[cookie_header],
        WRITE_TIMEOUT,
    )?;
    let email = resp
        .json()
        .and_then(|v| {
            v.get("Email")
                .or_else(|| v.get("UserPrincipalName"))
                .and_then(|s| s.as_str())
                .map(String::from)
        })
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| LiveError::upstream("SharePoint currentuser did not return an email address."))?;
    let personal = personal_segment_from_email(&email);
    let site_url = format!("{}/personal/{personal}", base_url.trim_end_matches('/'));
    let folder = format!("/personal/{personal}/Documents/Microsoft Teams Chat Files");
    Ok((site_url, folder))
}

fn resolve_channel_site_and_folder(base_url: &str, folder_rel: &str) -> Result<(String, String), LiveError> {
    let trimmed = folder_rel.trim();
    if !trimmed.starts_with('/') || trimmed.chars().any(|c| c.is_control()) {
        return Err(LiveError::bad(format!(
            "Invalid channel SharePoint folder path '{trimmed}'."
        )));
    }
    let parts: Vec<&str> = trimmed.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() < 3
        || !(parts[0].eq_ignore_ascii_case("sites") || parts[0].eq_ignore_ascii_case("teams"))
        || parts.iter().any(|p| matches!(*p, "." | ".."))
    {
        return Err(LiveError::bad(format!(
            "Cannot derive SharePoint site from channel folder '{trimmed}'."
        )));
    }
    let site_seg = crate::live::refs::percent_encode_segment(parts[1]);
    let site_url = format!("{}/{}/{}", base_url.trim_end_matches('/'), parts[0], site_seg);
    Ok((site_url, trimmed.to_string()))
}

fn create_share_link(
    transport: &Transport,
    site_url: &str,
    server_relative_url: &str,
    digest: &str,
    cookie_header: &str,
) -> (Option<String>, Option<String>) {
    let enc_file = encode_odata_string(server_relative_url);
    let url = format!(
        "{}/_api/web/GetFileByServerRelativeUrl('{enc_file}')/ListItemAllFields/ShareLink",
        site_url.trim_end_matches('/')
    );
    let body = json!({
        "request": {
            "createLink": true,
            "settings": {
                "allowAnonymousAccess": false,
                "linkKind": 3,
                "role": 2,
                "restrictShareMembership": false,
                "scope": 1,
            }
        }
    });
    let serialized = crate::json_codec::to_string(&body);
    let Ok(resp) = transport.request_bytes(
        "POST",
        &url,
        &[
            ("Cookie", cookie_header),
            ("Accept", "application/json;odata=nometadata"),
            ("Content-Type", "application/json;odata=nometadata"),
            ("X-RequestDigest", digest),
        ],
        Some(serialized.as_bytes()),
        &[cookie_header],
        WRITE_TIMEOUT,
    ) else {
        return (None, None);
    };
    let Some(v) = resp.json() else {
        return (None, None);
    };
    let info = v
        .get("sharingLinkInfo")
        .or_else(|| {
            v.get("d")
                .and_then(|d| d.get("ShareLink"))
                .and_then(|s| s.get("sharingLinkInfo"))
        })
        .unwrap_or(&v);
    let share_url = info
        .get("Url")
        .or_else(|| info.get("url"))
        .and_then(Value::as_str)
        .map(String::from);
    let share_id = info
        .get("ShareId")
        .or_else(|| info.get("shareId"))
        .and_then(Value::as_str)
        .map(String::from);
    (share_url, share_id)
}

pub fn upload_file(
    transport: &Transport,
    sp: &SharePointSession,
    channel_folder: Option<&str>,
    filename: &str,
    bytes: &[u8],
) -> Result<UploadedFile, LiveError> {
    if bytes.is_empty() {
        return Err(LiveError::bad(format!("File '{filename}' is empty.")));
    }
    if bytes.len() > MAX_SIMPLE_FILE_BYTES {
        return Err(LiveError::bad(format!(
            "File '{filename}' is {} bytes (max {} bytes for upload).",
            bytes.len(),
            MAX_SIMPLE_FILE_BYTES
        )));
    }

    let is_channel = channel_folder.is_some();
    let cookie_header = if is_channel {
        sp.team_cookie_header.as_deref().ok_or_else(|| {
            LiveError::auth(format!(
                "SharePoint session cookie for '{}' not found. Open a channel Files tab in Microsoft Teams, then try again.",
                sp.team_host
            ))
        })?
    } else {
        sp.personal_cookie_header.as_deref().ok_or_else(|| {
            LiveError::auth(format!(
                "OneDrive session cookie for '{}' not found. Open a chat file in Microsoft Teams, then try again.",
                sp.personal_host
            ))
        })?
    };

    let (base_origin, site_url, folder_rel) = if let Some(ch_folder) = channel_folder {
        let origin = transport
            .endpoints
            .sharepoint_base_override
            .clone()
            .unwrap_or_else(|| format!("https://{}", sp.team_host));
        let (site, folder) = resolve_channel_site_and_folder(&origin, ch_folder)?;
        (origin, site, folder)
    } else {
        let origin = transport
            .endpoints
            .sharepoint_base_override
            .clone()
            .unwrap_or_else(|| format!("https://{}", sp.personal_host));
        let (site, folder) = resolve_personal_site_and_folder(transport, &origin, cookie_header)?;
        (origin, site, folder)
    };

    let digest = fetch_digest(transport, &site_url, cookie_header)?;
    let enc_folder = encode_odata_string(&folder_rel);
    let enc_file = encode_odata_string(filename);
    let upload_url = format!(
        "{}/_api/web/GetFolderByServerRelativeUrl('{enc_folder}')/Files/add(url='{enc_file}',overwrite=true)",
        site_url.trim_end_matches('/')
    );
    let resp = transport.request_bytes(
        "POST",
        &upload_url,
        &[
            ("Cookie", cookie_header),
            ("Accept", "application/json;odata=nometadata"),
            ("X-RequestDigest", &digest),
        ],
        Some(bytes),
        &[cookie_header],
        WRITE_TIMEOUT,
    )?;

    let body_json = resp.json().unwrap_or(Value::Null);
    let item_id = body_json
        .get("UniqueId")
        .or_else(|| body_json.get("uniqueId"))
        .and_then(Value::as_str)
        .map(String::from)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| LiveError::upstream("SharePoint upload did not return UniqueId."))?;
    let server_rel = body_json
        .get("ServerRelativeUrl")
        .or_else(|| body_json.get("serverRelativeUrl"))
        .and_then(Value::as_str)
        .map(String::from)
        .unwrap_or_else(|| format!("{}/{filename}", folder_rel.trim_end_matches('/')));
    let final_name = body_json
        .get("Name")
        .and_then(Value::as_str)
        .unwrap_or(filename)
        .to_string();
    let file_type = final_name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default();

    let object_url = format!("{}{}", base_origin.trim_end_matches('/'), server_rel);
    let (share_url, share_id) = if is_channel {
        (None, None)
    } else {
        create_share_link(transport, &site_url, &server_rel, &digest, cookie_header)
    };

    Ok(UploadedFile {
        item_id,
        file_name: final_name,
        file_type,
        base_url: site_url.clone(),
        object_url,
        site_url,
        server_relative_url: server_rel,
        share_url,
        share_id,
        is_channel,
        size_bytes: bytes.len(),
    })
}
