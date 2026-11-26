use crate::daemon::pool::ReadConnPool;
use crate::live::compose::{self, BodyFormat, Importance};
use crate::live::error::LiveError;
use crate::live::policy::{Action, Principal, Scope};
use crate::live::service::chats::{ChatStateRequest, CreateChatRequest, MemberChangeRequest, RenameChatRequest};
use crate::live::service::messages::{
    EditMessageRequest, ForwardMessageRequest, MediaUpload, MessageActionRequest, ReactMessageRequest,
    SendMessageRequest, MAX_FILES_PER_MESSAGE, MAX_IMAGES_PER_MESSAGE,
};
use crate::live::service::{ArchiveSource, LiveService};
use base64::Engine;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;

pub const SMALL_JSON_BODY_CAP: usize = 64 * 1024;
pub const SEND_JSON_BODY_CAP: usize = 12 * 1024 * 1024;
pub const MAX_INLINE_DECODED_BYTES: usize = 8 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct InlineMediaBody {
    name: String,
    #[serde(default)]
    content_type: Option<String>,
    data_base64: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SendBody {
    target: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    reply_to: Option<String>,
    #[serde(default)]
    quote: Option<String>,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    importance: Option<String>,
    #[serde(default)]
    mentions: Vec<String>,
    #[serde(default)]
    images: Vec<InlineMediaBody>,
    #[serde(default)]
    files: Vec<InlineMediaBody>,
    #[serde(default)]
    client_message_id: Option<String>,
    #[serde(default)]
    dry_run: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct EditBody {
    #[serde(default)]
    chat: Option<String>,
    message_id: String,
    text: String,
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    dry_run: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct MessageActionBody {
    #[serde(default)]
    chat: Option<String>,
    message_id: String,
    #[serde(default)]
    dry_run: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ReactBody {
    #[serde(default)]
    chat: Option<String>,
    message_id: String,
    reaction: String,
    #[serde(default)]
    dry_run: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ForwardBody {
    #[serde(default)]
    from_chat: Option<String>,
    message_id: String,
    to_chat: String,
    #[serde(default)]
    comment: String,
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    dry_run: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct CreateChatBody {
    members: Vec<String>,
    #[serde(default)]
    topic: Option<String>,
    #[serde(default)]
    dry_run: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RenameChatBody {
    chat: String,
    topic: String,
    #[serde(default)]
    dry_run: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct MemberChangeBody {
    chat: String,
    member: String,
    #[serde(default)]
    dry_run: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ChatStateBody {
    chat: String,
    #[serde(default)]
    dry_run: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SetPresenceBody {
    availability: String,
    #[serde(default)]
    expires: Option<String>,
    #[serde(default)]
    dry_run: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SetStatusMessageBody {
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    expires: Option<String>,
    #[serde(default)]
    clear: bool,
    #[serde(default)]
    dry_run: bool,
}

pub fn route_spec(method: &str, path: &str) -> Option<(Scope, bool, usize)> {
    match (method, path) {
        ("GET", "/v1/live/status") => Some((Scope::Read, false, 0)),
        ("GET", "/v1/live/presence") => Some((Scope::PresenceRead, false, 0)),
        ("POST", "/v1/live/messages/send") => Some((Action::SendMessage.required_scope(), true, SEND_JSON_BODY_CAP)),
        ("POST", "/v1/live/messages/edit") => Some((Action::EditMessage.required_scope(), true, SMALL_JSON_BODY_CAP)),
        ("POST", "/v1/live/messages/delete") => {
            Some((Action::DeleteMessage.required_scope(), true, SMALL_JSON_BODY_CAP))
        }
        ("POST", "/v1/live/messages/react") => Some((Action::React.required_scope(), true, SMALL_JSON_BODY_CAP)),
        ("POST", "/v1/live/messages/unreact") => Some((Action::Unreact.required_scope(), true, SMALL_JSON_BODY_CAP)),
        ("POST", "/v1/live/messages/pin") => Some((Action::Pin.required_scope(), true, SMALL_JSON_BODY_CAP)),
        ("POST", "/v1/live/messages/unpin") => Some((Action::Unpin.required_scope(), true, SMALL_JSON_BODY_CAP)),
        ("POST", "/v1/live/messages/save") => Some((Action::Save.required_scope(), true, SMALL_JSON_BODY_CAP)),
        ("POST", "/v1/live/messages/unsave") => Some((Action::Unsave.required_scope(), true, SMALL_JSON_BODY_CAP)),
        ("POST", "/v1/live/messages/forward") => {
            Some((Action::ForwardMessage.required_scope(), true, SMALL_JSON_BODY_CAP))
        }
        ("POST", "/v1/live/chats/create") => Some((Action::CreateChat.required_scope(), true, SMALL_JSON_BODY_CAP)),
        ("POST", "/v1/live/chats/rename") => Some((Action::RenameChat.required_scope(), true, SMALL_JSON_BODY_CAP)),
        ("POST", "/v1/live/chats/add-member") => Some((Action::AddMember.required_scope(), true, SMALL_JSON_BODY_CAP)),
        ("POST", "/v1/live/chats/remove-member") => {
            Some((Action::RemoveMember.required_scope(), true, SMALL_JSON_BODY_CAP))
        }
        ("POST", "/v1/live/chats/read") => Some((Action::MarkRead.required_scope(), true, SMALL_JSON_BODY_CAP)),
        ("POST", "/v1/live/chats/unread") => Some((Action::MarkUnread.required_scope(), true, SMALL_JSON_BODY_CAP)),
        ("POST", "/v1/live/chats/mute") => Some((Action::MuteChat.required_scope(), true, SMALL_JSON_BODY_CAP)),
        ("POST", "/v1/live/chats/unmute") => Some((Action::UnmuteChat.required_scope(), true, SMALL_JSON_BODY_CAP)),
        ("POST", "/v1/live/presence/set") => Some((Action::SetPresence.required_scope(), true, SMALL_JSON_BODY_CAP)),
        ("POST", "/v1/live/presence/message") => {
            Some((Action::SetStatusMessage.required_scope(), true, SMALL_JSON_BODY_CAP))
        }
        _ => None,
    }
}

fn parse_format(raw: Option<&str>) -> Result<BodyFormat, LiveError> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None | Some("plain") => Ok(BodyFormat::Plain),
        Some("markdown") | Some("md") => Ok(BodyFormat::Markdown),
        Some("html") => Err(LiveError::bad(
            "Raw HTML format is CLI-only; use 'plain' or 'markdown'.",
        )),
        Some(other) => Err(LiveError::bad(format!(
            "Invalid format '{other}'. Use 'plain' or 'markdown'."
        ))),
    }
}

fn parse_importance(raw: Option<&str>) -> Result<Importance, LiveError> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None | Some("normal") => Ok(Importance::Normal),
        Some("high") | Some("important") => Ok(Importance::High),
        Some("urgent") => Ok(Importance::Urgent),
        Some(other) => Err(LiveError::bad(format!(
            "Invalid importance '{other}'. Use 'normal', 'high', or 'urgent'."
        ))),
    }
}

fn decode_media(items: Vec<InlineMediaBody>, total_decoded: &mut usize) -> Result<Vec<MediaUpload>, LiveError> {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let name = item.name.trim().to_string();
        if name.is_empty()
            || matches!(name.as_str(), "." | "..")
            || name.contains('/')
            || name.contains('\\')
            || name.chars().any(|c| c.is_control())
        {
            return Err(LiveError::bad("Attachment filename must be a simple file name."));
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(item.data_base64.trim())
            .map_err(|e| LiveError::bad(format!("Invalid base64 for '{}': {e}", name)))?;
        *total_decoded = total_decoded.saturating_add(bytes.len());
        if *total_decoded > MAX_INLINE_DECODED_BYTES {
            return Err(LiveError::bad(format!(
                "Total decoded attachment bytes exceed {} bytes.",
                MAX_INLINE_DECODED_BYTES
            )));
        }
        let ct = match item.content_type {
            Some(raw) if !raw.trim().is_empty() => {
                if raw.chars().any(|c| c.is_control()) {
                    return Err(LiveError::bad("Invalid contentType header value."));
                }
                let bare = compose::normalize_mime_type(&raw);
                if bare.is_empty() || !bare.contains('/') {
                    return Err(LiveError::bad(format!("Invalid contentType '{raw}'.")));
                }
                bare
            }
            _ => compose::guess_mime_from_filename(&name).to_string(),
        };
        out.push(MediaUpload {
            filename: name,
            content_type: ct,
            bytes,
        });
    }
    Ok(out)
}

fn parse_json_body<T: serde::de::DeserializeOwned>(raw: &[u8]) -> Result<T, LiveError> {
    serde_json::from_slice(raw).map_err(|e| LiveError::bad(format!("Invalid JSON body: {e}")))
}

#[allow(clippy::too_many_arguments)]
pub fn dispatch(
    live: &LiveService,
    pool: &ReadConnPool,
    principal: &Principal,
    method: &str,
    path: &str,
    q: &HashMap<String, String>,
    body: &[u8],
    idempotency_key: Option<String>,
) -> Result<Value, LiveError> {
    let archive = ArchiveSource::Pool(pool);
    match (method, path) {
        ("GET", "/v1/live/status") => Ok(live.status()),
        ("GET", "/v1/live/presence") => {
            let people: Vec<String> = q
                .get("person")
                .map(|s| {
                    s.split(',')
                        .map(str::trim)
                        .filter(|p| !p.is_empty())
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default();
            live.get_presence(archive, principal, &people)
        }
        ("POST", "/v1/live/messages/send") => {
            let b: SendBody = parse_json_body(body)?;
            let format = parse_format(b.format.as_deref())?;
            let importance = parse_importance(b.importance.as_deref())?;
            if b.images.len() > MAX_IMAGES_PER_MESSAGE {
                return Err(LiveError::bad(format!(
                    "Too many images ({}; max {MAX_IMAGES_PER_MESSAGE} per message).",
                    b.images.len()
                )));
            }
            if b.files.len() > MAX_FILES_PER_MESSAGE {
                return Err(LiveError::bad(format!(
                    "Too many files ({}; max {MAX_FILES_PER_MESSAGE} per message).",
                    b.files.len()
                )));
            }
            let mut total_decoded = 0usize;
            let images = decode_media(b.images, &mut total_decoded)?;
            let files = decode_media(b.files, &mut total_decoded)?;
            live.send_message(
                archive,
                principal,
                SendMessageRequest {
                    target: b.target,
                    text: b.text,
                    format,
                    reply_to: b.reply_to,
                    quote: b.quote,
                    subject: b.subject,
                    importance,
                    mentions: b.mentions,
                    images,
                    files,
                    client_message_id: b.client_message_id,
                    idempotency_key,
                    dry_run: b.dry_run,
                },
            )
        }
        ("POST", "/v1/live/messages/edit") => {
            let b: EditBody = parse_json_body(body)?;
            let format = parse_format(b.format.as_deref())?;
            live.edit_message(
                archive,
                principal,
                EditMessageRequest {
                    chat: b.chat,
                    message_id: b.message_id,
                    text: b.text,
                    format,
                    dry_run: b.dry_run,
                },
            )
        }
        ("POST", "/v1/live/messages/delete") => {
            let b: MessageActionBody = parse_json_body(body)?;
            live.delete_message(
                archive,
                principal,
                MessageActionRequest {
                    chat: b.chat,
                    message_id: b.message_id,
                    dry_run: b.dry_run,
                },
            )
        }
        ("POST", "/v1/live/messages/react") => {
            let b: ReactBody = parse_json_body(body)?;
            live.react_message(
                archive,
                principal,
                ReactMessageRequest {
                    chat: b.chat,
                    message_id: b.message_id,
                    reaction: b.reaction,
                    add: true,
                    dry_run: b.dry_run,
                },
            )
        }
        ("POST", "/v1/live/messages/unreact") => {
            let b: ReactBody = parse_json_body(body)?;
            live.react_message(
                archive,
                principal,
                ReactMessageRequest {
                    chat: b.chat,
                    message_id: b.message_id,
                    reaction: b.reaction,
                    add: false,
                    dry_run: b.dry_run,
                },
            )
        }
        ("POST", "/v1/live/messages/pin") => {
            let b: MessageActionBody = parse_json_body(body)?;
            live.pin_message(
                archive,
                principal,
                MessageActionRequest {
                    chat: b.chat,
                    message_id: b.message_id,
                    dry_run: b.dry_run,
                },
                true,
            )
        }
        ("POST", "/v1/live/messages/unpin") => {
            let b: MessageActionBody = parse_json_body(body)?;
            live.pin_message(
                archive,
                principal,
                MessageActionRequest {
                    chat: b.chat,
                    message_id: b.message_id,
                    dry_run: b.dry_run,
                },
                false,
            )
        }
        ("POST", "/v1/live/messages/save") => {
            let b: MessageActionBody = parse_json_body(body)?;
            live.save_message(
                archive,
                principal,
                MessageActionRequest {
                    chat: b.chat,
                    message_id: b.message_id,
                    dry_run: b.dry_run,
                },
                true,
            )
        }
        ("POST", "/v1/live/messages/unsave") => {
            let b: MessageActionBody = parse_json_body(body)?;
            live.save_message(
                archive,
                principal,
                MessageActionRequest {
                    chat: b.chat,
                    message_id: b.message_id,
                    dry_run: b.dry_run,
                },
                false,
            )
        }
        ("POST", "/v1/live/messages/forward") => {
            let b: ForwardBody = parse_json_body(body)?;
            let format = parse_format(b.format.as_deref())?;
            live.forward_message(
                archive,
                principal,
                ForwardMessageRequest {
                    from_chat: b.from_chat,
                    message_id: b.message_id,
                    to_chat: b.to_chat,
                    comment: b.comment,
                    format,
                    idempotency_key,
                    dry_run: b.dry_run,
                },
            )
        }
        ("POST", "/v1/live/chats/create") => {
            let b: CreateChatBody = parse_json_body(body)?;
            live.create_chat(
                archive,
                principal,
                CreateChatRequest {
                    members: b.members,
                    topic: b.topic,
                    dry_run: b.dry_run,
                },
            )
        }
        ("POST", "/v1/live/chats/rename") => {
            let b: RenameChatBody = parse_json_body(body)?;
            live.rename_chat(
                archive,
                principal,
                RenameChatRequest {
                    chat: b.chat,
                    topic: b.topic,
                    dry_run: b.dry_run,
                },
            )
        }
        ("POST", "/v1/live/chats/add-member") => {
            let b: MemberChangeBody = parse_json_body(body)?;
            live.change_member(
                archive,
                principal,
                MemberChangeRequest {
                    chat: b.chat,
                    member: b.member,
                    add: true,
                    dry_run: b.dry_run,
                },
            )
        }
        ("POST", "/v1/live/chats/remove-member") => {
            let b: MemberChangeBody = parse_json_body(body)?;
            live.change_member(
                archive,
                principal,
                MemberChangeRequest {
                    chat: b.chat,
                    member: b.member,
                    add: false,
                    dry_run: b.dry_run,
                },
            )
        }
        ("POST", "/v1/live/chats/read") => {
            let b: ChatStateBody = parse_json_body(body)?;
            live.set_read_state(
                archive,
                principal,
                ChatStateRequest {
                    chat: b.chat,
                    dry_run: b.dry_run,
                },
                true,
            )
        }
        ("POST", "/v1/live/chats/unread") => {
            let b: ChatStateBody = parse_json_body(body)?;
            live.set_read_state(
                archive,
                principal,
                ChatStateRequest {
                    chat: b.chat,
                    dry_run: b.dry_run,
                },
                false,
            )
        }
        ("POST", "/v1/live/chats/mute") => {
            let b: ChatStateBody = parse_json_body(body)?;
            live.set_mute_state(
                archive,
                principal,
                ChatStateRequest {
                    chat: b.chat,
                    dry_run: b.dry_run,
                },
                true,
            )
        }
        ("POST", "/v1/live/chats/unmute") => {
            let b: ChatStateBody = parse_json_body(body)?;
            live.set_mute_state(
                archive,
                principal,
                ChatStateRequest {
                    chat: b.chat,
                    dry_run: b.dry_run,
                },
                false,
            )
        }
        ("POST", "/v1/live/presence/set") => {
            let b: SetPresenceBody = parse_json_body(body)?;
            live.set_presence(principal, &b.availability, b.expires.as_deref(), b.dry_run)
        }
        ("POST", "/v1/live/presence/message") => {
            let b: SetStatusMessageBody = parse_json_body(body)?;
            live.set_status_message(
                principal,
                b.message.as_deref(),
                b.clear,
                b.expires.as_deref(),
                b.dry_run,
            )
        }
        _ => Err(LiveError::not_found(format!("No live route {method} {path}"))),
    }
}
