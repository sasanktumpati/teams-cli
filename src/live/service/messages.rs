use crate::live::clients::{ams, chatsvc, sharepoint};
use crate::live::compose::{self, BodyFormat, ComposeInput, ForwardSpec, Importance, MentionSpec, QuoteSpec};
use crate::live::error::LiveError;
use crate::live::policy::{self, Action, Principal, Scope};
use crate::live::refs::{MessageRef, UploadedFile, UploadedImage};
use crate::live::service::{resolve_scope, ArchiveSource, LiveService};
use crate::query::resolve::{self, Strictness};
use regex::Regex;
use serde_json::{json, Value};
use std::sync::LazyLock;

static REPLY_TO_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9]+$").unwrap());

pub const MAX_IMAGES_PER_MESSAGE: usize = 10;
pub const MAX_FILES_PER_MESSAGE: usize = 10;

#[derive(Debug, Clone)]
pub struct MediaUpload {
    pub filename: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct SendMessageRequest {
    pub target: String,
    pub text: String,
    pub format: BodyFormat,
    pub reply_to: Option<String>,
    pub quote: Option<String>,
    pub subject: Option<String>,
    pub importance: Importance,
    pub mentions: Vec<String>,
    pub images: Vec<MediaUpload>,
    pub files: Vec<MediaUpload>,
    pub client_message_id: Option<String>,
    pub idempotency_key: Option<String>,
    pub dry_run: bool,
}

#[derive(Debug, Clone)]
pub struct EditMessageRequest {
    pub chat: Option<String>,
    pub message_id: String,
    pub text: String,
    pub format: BodyFormat,
    pub dry_run: bool,
}

#[derive(Debug, Clone)]
pub struct MessageActionRequest {
    pub chat: Option<String>,
    pub message_id: String,
    pub dry_run: bool,
}

#[derive(Debug, Clone)]
pub struct ReactMessageRequest {
    pub chat: Option<String>,
    pub message_id: String,
    pub reaction: String,
    pub add: bool,
    pub dry_run: bool,
}

#[derive(Debug, Clone)]
pub struct ForwardMessageRequest {
    pub from_chat: Option<String>,
    pub message_id: String,
    pub to_chat: String,
    pub comment: String,
    pub format: BodyFormat,
    pub idempotency_key: Option<String>,
    pub dry_run: bool,
}

fn validate_uploads(images: &[MediaUpload], files: &[MediaUpload]) -> Result<(), LiveError> {
    if images.len() > MAX_IMAGES_PER_MESSAGE {
        return Err(LiveError::bad(format!(
            "Too many images ({}; max {MAX_IMAGES_PER_MESSAGE} per message).",
            images.len()
        )));
    }
    if files.len() > MAX_FILES_PER_MESSAGE {
        return Err(LiveError::bad(format!(
            "Too many files ({}; max {MAX_FILES_PER_MESSAGE} per message).",
            files.len()
        )));
    }
    for img in images {
        if img.bytes.is_empty() {
            return Err(LiveError::bad(format!("Image '{}' is empty.", img.filename)));
        }
        if img.bytes.len() > ams::MAX_IMAGE_BYTES {
            return Err(LiveError::bad(format!(
                "Image '{}' exceeds max size of {} bytes.",
                img.filename,
                ams::MAX_IMAGE_BYTES
            )));
        }
        if !compose::is_supported_image_mime(&img.content_type) {
            return Err(LiveError::bad(format!(
                "Unsupported image content type '{}' for '{}'.",
                img.content_type, img.filename
            )));
        }
    }
    for f in files {
        if f.bytes.is_empty() {
            return Err(LiveError::bad(format!("File '{}' is empty.", f.filename)));
        }
        if f.bytes.len() > sharepoint::MAX_SIMPLE_FILE_BYTES {
            return Err(LiveError::bad(format!(
                "File '{}' exceeds max size of {} bytes.",
                f.filename,
                sharepoint::MAX_SIMPLE_FILE_BYTES
            )));
        }
    }
    Ok(())
}

impl LiveService {
    pub fn send_message(
        &self,
        archive: ArchiveSource<'_>,
        principal: &Principal,
        req: SendMessageRequest,
    ) -> Result<Value, LiveError> {
        policy::authorize_principal(principal, Action::SendMessage, None)?;
        if !principal.is_cli && req.format == BodyFormat::Html {
            return Err(LiveError::bad(
                "Raw HTML mode is CLI-only; use plain or markdown over HTTP.",
            ));
        }
        if let Some(ref rt) = req.reply_to {
            if !REPLY_TO_RE.is_match(rt) {
                return Err(LiveError::bad("reply_to must be a numeric message id."));
            }
        }
        let quote_id = req.quote.as_deref().map(str::trim).filter(|s| !s.is_empty());
        if quote_id.is_some() && !principal.is_cli && !principal.has_scope(Scope::Read) {
            return Err(LiveError::forbidden(
                "scope_required",
                format!(
                    "Token '{}' lacks required scope 'read' to read archived messages for quoting.",
                    principal.name
                ),
            )
            .with_details(json!({ "requiredScope": "read" })));
        }
        validate_uploads(&req.images, &req.files)?;

        let scope = resolve_scope(principal);
        let (resolved, display_name, mentions, quote) = archive.with_conn(|conn| {
            let (chat, _) = resolve::resolve_chat_scoped(conn, &req.target, Strictness::Strict, scope)?;
            policy::authorize_principal(principal, Action::SendMessage, Some(&chat.id))?;
            let disp = resolve::me_display_name(conn);
            let mut mspecs: Vec<MentionSpec> = Vec::with_capacity(req.mentions.len());
            for mq in &req.mentions {
                let (p, _) = resolve::resolve_person_scoped(conn, mq, Strictness::Strict, scope)?;
                if !mspecs
                    .iter()
                    .any(|existing| crate::identity::same_person(Some(&existing.mri), Some(&p.mri)))
                {
                    mspecs.push(MentionSpec::person(p.mri, p.name));
                }
            }
            let qspec = match quote_id {
                Some(q_id) => {
                    let m = resolve::resolve_message_scoped(conn, Some(&chat.id), q_id, scope)?;
                    if !principal.is_cli && !principal.allows_conversation(&m.conversation_id) {
                        return Err(LiveError::forbidden(
                            "chat_not_allowed",
                            format!(
                                "Quoted conversation {:?} is not in token '{}' allowlist.",
                                m.conversation_id, principal.name
                            ),
                        ));
                    }
                    if m.conversation_id != chat.id {
                        return Err(LiveError::bad("Quoted message must belong to the target conversation."));
                    }
                    if !m.archived {
                        return Err(LiveError::not_found(format!(
                            "Quoted message '{q_id}' not found in archive; run `teams sync` first."
                        )));
                    }
                    Some(QuoteSpec {
                        message_id: m.message_id,
                        sender_mri: m.sender_mri,
                        sender_name: m.sender_name,
                        arrived_at_ms: m.arrived_at,
                        text: m.text,
                    })
                }
                None => None,
            };
            Ok((chat, disp, mspecs, qspec))
        })?;

        let auth_check = policy::authorize(
            principal,
            Action::SendMessage,
            Some(&resolved.id),
            self.daemon_writes_enabled,
        );
        let allowed_conv = if principal.is_cli {
            policy::is_conversation_globally_allowed(&resolved.id)
        } else {
            principal.allows_conversation(&resolved.id) && policy::is_conversation_globally_allowed(&resolved.id)
        };
        let writes_enabled = if principal.is_cli {
            policy::is_cli_writes_enabled()
        } else {
            self.daemon_writes_enabled
        };

        let ch_folder = if !req.files.is_empty() && resolved.kind == "channel" {
            let folder = resolved.sharepoint_folder.as_deref().ok_or_else(|| {
                LiveError::bad(
                    "Channel SharePoint folder not found in archive; run `teams sync` or verify the channel has a Files tab.",
                )
            })?;
            Some(folder)
        } else {
            None
        };

        if req.dry_run {
            let preview_images: Vec<UploadedImage> = req
                .images
                .iter()
                .enumerate()
                .map(|(i, img)| UploadedImage {
                    object_id: format!("dry-run-image-{i}"),
                    imgo_url: format!("https://asm.skype.com/v1/objects/dry-run-image-{i}/views/imgo"),
                    filename: img.filename.clone(),
                    content_type: img.content_type.clone(),
                    size_bytes: img.bytes.len(),
                })
                .collect();
            let preview_files: Vec<UploadedFile> = req
                .files
                .iter()
                .enumerate()
                .map(|(i, f)| UploadedFile {
                    item_id: format!("dry-run-file-{i}"),
                    file_name: f.filename.clone(),
                    file_type: f
                        .filename
                        .rsplit_once('.')
                        .map(|(_, e)| e.to_ascii_lowercase())
                        .unwrap_or_default(),
                    base_url: "https://sharepoint.example/dry-run".to_string(),
                    object_url: format!("https://sharepoint.example/dry-run/{}", f.filename),
                    site_url: "https://sharepoint.example/dry-run".to_string(),
                    server_relative_url: format!("/dry-run/{}", f.filename),
                    share_url: None,
                    share_id: None,
                    is_channel: resolved.kind == "channel",
                    size_bytes: f.bytes.len(),
                })
                .collect();
            let composed = compose::compose_message(&ComposeInput {
                text: req.text.clone(),
                format: req.format,
                display_name,
                subject: req.subject.clone(),
                importance: req.importance,
                mentions: mentions.clone(),
                quote: quote.clone(),
                forward: None,
                images: preview_images,
                files: preview_files,
                client_message_id: req.client_message_id.clone(),
                idempotency_key: req.idempotency_key.clone(),
            })?;
            return Ok(json!({
                "ok": true,
                "dryRun": true,
                "sent": false,
                "archiveVisible": false,
                "restricted": !writes_enabled,
                "allowed": allowed_conv,
                "conversationId": resolved.id,
                "chatName": resolved.name,
                "chatKind": resolved.kind,
                "replyTo": req.reply_to,
                "quoteMessageId": quote.as_ref().map(|q| q.message_id.as_str()),
                "subject": req.subject,
                "importance": req.importance.as_teams_str(),
                "mentions": mentions.iter().map(|m| json!({"mri": m.mri, "name": m.display_name})).collect::<Vec<_>>(),
                "images": req.images.iter().map(|i| json!({"name": i.filename, "contentType": i.content_type, "sizeBytes": i.bytes.len()})).collect::<Vec<_>>(),
                "files": req.files.iter().map(|f| json!({"name": f.filename, "contentType": f.content_type, "sizeBytes": f.bytes.len()})).collect::<Vec<_>>(),
                "text": req.text,
                "content": composed.content_html,
                "clientMessageId": composed.client_message_id,
            }));
        }

        auth_check?;
        self.check_write_rate_limit(principal, Action::SendMessage)?;

        let tok = self.session.skypetoken()?;
        let eff_display = if display_name.is_empty() {
            tok.display_name.clone()
        } else {
            display_name
        };

        let mut uploaded_images = Vec::with_capacity(req.images.len());
        for img in &req.images {
            let ct = compose::normalize_mime_type(&img.content_type);
            let up = self.track_upstream(ams::upload_image(
                &self.transport,
                &tok,
                &resolved.id,
                &img.filename,
                &ct,
                &img.bytes,
            ))?;
            uploaded_images.push(up);
        }

        let mut uploaded_files = Vec::with_capacity(req.files.len());
        if !req.files.is_empty() {
            let sp = self.session.sharepoint_session_for(ch_folder.is_some())?;
            for f in &req.files {
                let up = self.track_upstream(sharepoint::upload_file(
                    &self.transport,
                    &sp,
                    ch_folder,
                    &f.filename,
                    &f.bytes,
                ))?;
                uploaded_files.push(up);
            }
        }

        let images_summary: Vec<Value> = uploaded_images
            .iter()
            .map(|i| json!({"objectId": i.object_id, "name": i.filename, "sizeBytes": i.size_bytes}))
            .collect();
        let files_summary: Vec<Value> = uploaded_files
            .iter()
            .map(|f| json!({"itemId": f.item_id, "name": f.file_name, "url": f.share_url.as_deref().unwrap_or(&f.object_url), "sizeBytes": f.size_bytes}))
            .collect();

        let composed = compose::compose_message(&ComposeInput {
            text: req.text.clone(),
            format: req.format,
            display_name: eff_display,
            subject: req.subject.clone(),
            importance: req.importance,
            mentions,
            quote: quote.clone(),
            forward: None,
            images: uploaded_images,
            files: uploaded_files,
            client_message_id: req.client_message_id.clone(),
            idempotency_key: req.idempotency_key.clone(),
        })?;

        let (status, msg_id) = self.track_upstream(chatsvc::send_message(
            &self.transport,
            &tok,
            &resolved.id,
            req.reply_to.as_deref(),
            &composed.payload,
        ))?;

        let hash = policy::content_sha256_hex(&composed.content_html);
        policy::append_audit_entry(
            principal,
            Action::SendMessage,
            Some(&resolved.id),
            msg_id.as_deref(),
            Some(&hash),
        );

        Ok(json!({
            "ok": true,
            "dryRun": false,
            "sent": true,
            "archiveVisible": false,
            "status": status,
            "messageId": msg_id,
            "conversationId": resolved.id,
            "chatName": resolved.name,
            "chatKind": resolved.kind,
            "replyTo": req.reply_to,
            "quoteMessageId": quote.as_ref().map(|q| q.message_id.as_str()),
            "subject": req.subject,
            "importance": req.importance.as_teams_str(),
            "clientMessageId": composed.client_message_id,
            "imagesUploaded": images_summary,
            "filesUploaded": files_summary,
            "text": req.text,
            "region": tok.region,
        }))
    }

    pub fn edit_message(
        &self,
        archive: ArchiveSource<'_>,
        principal: &Principal,
        req: EditMessageRequest,
    ) -> Result<Value, LiveError> {
        policy::authorize_principal(principal, Action::EditMessage, None)?;
        if !principal.is_cli && req.format == BodyFormat::Html {
            return Err(LiveError::bad(
                "Raw HTML mode is CLI-only; use plain or markdown over HTTP.",
            ));
        }
        if req.text.trim().is_empty() {
            return Err(LiveError::bad("Edited message text cannot be empty."));
        }

        let scope = resolve_scope(principal);
        let (msg_ref, me_mri) = archive.with_conn(|conn| {
            let m = resolve::resolve_message_scoped(conn, req.chat.as_deref(), &req.message_id, scope)?;
            let me = resolve::me_mri(conn);
            Ok((MessageRef::from_resolved(&m), me))
        })?;

        policy::authorize_principal(principal, Action::EditMessage, Some(&msg_ref.conversation_id))?;

        if let (Some(ref me), false) = (&me_mri, msg_ref.sender_mri.is_empty()) {
            if !crate::identity::same_person(Some(&msg_ref.sender_mri), Some(me)) {
                return Err(LiveError::forbidden(
                    "not_message_author",
                    "Refusing to edit a message sent by someone else.",
                ));
            }
        }

        let content_html = match req.format {
            BodyFormat::Plain => compose::format_plain_html(&req.text),
            BodyFormat::Markdown => compose::render_markdown_to_teams_html(&req.text),
            BodyFormat::Html => req.text.clone(),
        };

        let auth_check = policy::authorize(
            principal,
            Action::EditMessage,
            Some(&msg_ref.conversation_id),
            self.daemon_writes_enabled,
        );

        if req.dry_run {
            return Ok(json!({
                "ok": true,
                "dryRun": true,
                "edited": false,
                "archiveVisible": false,
                "conversationId": msg_ref.conversation_id,
                "chatName": msg_ref.chat_name,
                "messageId": msg_ref.message_id,
                "threadRoot": msg_ref.thread_root,
                "content": content_html,
                "text": req.text,
            }));
        }

        auth_check?;
        self.check_write_rate_limit(principal, Action::EditMessage)?;

        let tok = self.session.skypetoken()?;
        let status = self.track_upstream(chatsvc::edit_message(
            &self.transport,
            &tok,
            &msg_ref,
            &content_html,
            None,
        ))?;

        let hash = policy::content_sha256_hex(&content_html);
        policy::append_audit_entry(
            principal,
            Action::EditMessage,
            Some(&msg_ref.conversation_id),
            Some(&msg_ref.message_id),
            Some(&hash),
        );

        Ok(json!({
            "ok": true,
            "dryRun": false,
            "edited": true,
            "archiveVisible": false,
            "status": status,
            "conversationId": msg_ref.conversation_id,
            "chatName": msg_ref.chat_name,
            "messageId": msg_ref.message_id,
            "threadRoot": msg_ref.thread_root,
            "text": req.text,
        }))
    }

    pub fn delete_message(
        &self,
        archive: ArchiveSource<'_>,
        principal: &Principal,
        req: MessageActionRequest,
    ) -> Result<Value, LiveError> {
        policy::authorize_principal(principal, Action::DeleteMessage, None)?;
        let scope = resolve_scope(principal);
        let (msg_ref, me_mri) = archive.with_conn(|conn| {
            let m = resolve::resolve_message_scoped(conn, req.chat.as_deref(), &req.message_id, scope)?;
            let me = resolve::me_mri(conn);
            Ok((MessageRef::from_resolved(&m), me))
        })?;

        policy::authorize_principal(principal, Action::DeleteMessage, Some(&msg_ref.conversation_id))?;

        if let (Some(ref me), false) = (&me_mri, msg_ref.sender_mri.is_empty()) {
            if !crate::identity::same_person(Some(&msg_ref.sender_mri), Some(me)) {
                return Err(LiveError::forbidden(
                    "not_message_author",
                    "Refusing to delete a message sent by someone else.",
                ));
            }
        }

        let auth_check = policy::authorize(
            principal,
            Action::DeleteMessage,
            Some(&msg_ref.conversation_id),
            self.daemon_writes_enabled,
        );

        if req.dry_run {
            return Ok(json!({
                "ok": true,
                "dryRun": true,
                "deleted": false,
                "archiveVisible": false,
                "conversationId": msg_ref.conversation_id,
                "chatName": msg_ref.chat_name,
                "messageId": msg_ref.message_id,
                "threadRoot": msg_ref.thread_root,
            }));
        }

        auth_check?;
        self.check_write_rate_limit(principal, Action::DeleteMessage)?;

        let tok = self.session.skypetoken()?;
        let status = self.track_upstream(chatsvc::delete_message(&self.transport, &tok, &msg_ref))?;
        policy::append_audit_entry(
            principal,
            Action::DeleteMessage,
            Some(&msg_ref.conversation_id),
            Some(&msg_ref.message_id),
            None,
        );

        Ok(json!({
            "ok": true,
            "dryRun": false,
            "deleted": true,
            "archiveVisible": false,
            "status": status,
            "conversationId": msg_ref.conversation_id,
            "chatName": msg_ref.chat_name,
            "messageId": msg_ref.message_id,
            "threadRoot": msg_ref.thread_root,
        }))
    }

    pub fn react_message(
        &self,
        archive: ArchiveSource<'_>,
        principal: &Principal,
        req: ReactMessageRequest,
    ) -> Result<Value, LiveError> {
        let action = if req.add { Action::React } else { Action::Unreact };
        policy::authorize_principal(principal, action, None)?;
        let emotion_key = compose::normalize_reaction_key(&req.reaction)?;
        let scope = resolve_scope(principal);
        let msg_ref = archive.with_conn(|conn| {
            let m = resolve::resolve_message_scoped(conn, req.chat.as_deref(), &req.message_id, scope)?;
            Ok(MessageRef::from_resolved(&m))
        })?;

        policy::authorize_principal(principal, action, Some(&msg_ref.conversation_id))?;
        let auth_check = policy::authorize(
            principal,
            action,
            Some(&msg_ref.conversation_id),
            self.daemon_writes_enabled,
        );

        if req.dry_run {
            return Ok(json!({
                "ok": true,
                "dryRun": true,
                "applied": false,
                "archiveVisible": false,
                "action": if req.add { "react" } else { "unreact" },
                "reaction": emotion_key,
                "conversationId": msg_ref.conversation_id,
                "chatName": msg_ref.chat_name,
                "messageId": msg_ref.message_id,
            }));
        }

        auth_check?;
        self.check_write_rate_limit(principal, action)?;

        let tok = self.session.skypetoken()?;
        let status = self.track_upstream(chatsvc::set_reaction(
            &self.transport,
            &tok,
            &msg_ref,
            &emotion_key,
            req.add,
        ))?;
        policy::append_audit_entry(
            principal,
            action,
            Some(&msg_ref.conversation_id),
            Some(&msg_ref.message_id),
            None,
        );

        Ok(json!({
            "ok": true,
            "dryRun": false,
            "applied": true,
            "archiveVisible": false,
            "status": status,
            "action": if req.add { "react" } else { "unreact" },
            "reaction": emotion_key,
            "conversationId": msg_ref.conversation_id,
            "chatName": msg_ref.chat_name,
            "messageId": msg_ref.message_id,
        }))
    }

    pub fn pin_message(
        &self,
        archive: ArchiveSource<'_>,
        principal: &Principal,
        req: MessageActionRequest,
        pin: bool,
    ) -> Result<Value, LiveError> {
        let action = if pin { Action::Pin } else { Action::Unpin };
        policy::authorize_principal(principal, action, None)?;
        let scope = resolve_scope(principal);
        let msg_ref = archive.with_conn(|conn| {
            let m = resolve::resolve_message_scoped(conn, req.chat.as_deref(), &req.message_id, scope)?;
            Ok(MessageRef::from_resolved(&m))
        })?;
        policy::authorize_principal(principal, action, Some(&msg_ref.conversation_id))?;
        let auth_check = policy::authorize(
            principal,
            action,
            Some(&msg_ref.conversation_id),
            self.daemon_writes_enabled,
        );

        if req.dry_run {
            return Ok(json!({
                "ok": true,
                "dryRun": true,
                "applied": false,
                "archiveVisible": false,
                "pinned": pin,
                "conversationId": msg_ref.conversation_id,
                "chatName": msg_ref.chat_name,
                "messageId": msg_ref.message_id,
            }));
        }

        auth_check?;
        self.check_write_rate_limit(principal, action)?;

        let tok = self.session.skypetoken()?;
        let status = self.track_upstream(chatsvc::set_pinned(&self.transport, &tok, &msg_ref, pin))?;
        policy::append_audit_entry(
            principal,
            action,
            Some(&msg_ref.conversation_id),
            Some(&msg_ref.message_id),
            None,
        );

        Ok(json!({
            "ok": true,
            "dryRun": false,
            "applied": true,
            "archiveVisible": false,
            "status": status,
            "pinned": pin,
            "conversationId": msg_ref.conversation_id,
            "chatName": msg_ref.chat_name,
            "messageId": msg_ref.message_id,
        }))
    }

    pub fn save_message(
        &self,
        archive: ArchiveSource<'_>,
        principal: &Principal,
        req: MessageActionRequest,
        save: bool,
    ) -> Result<Value, LiveError> {
        let action = if save { Action::Save } else { Action::Unsave };
        policy::authorize_principal(principal, action, None)?;
        let scope = resolve_scope(principal);
        let msg_ref = archive.with_conn(|conn| {
            let m = resolve::resolve_message_scoped(conn, req.chat.as_deref(), &req.message_id, scope)?;
            Ok(MessageRef::from_resolved(&m))
        })?;
        policy::authorize_principal(principal, action, Some(&msg_ref.conversation_id))?;
        let auth_check = policy::authorize(
            principal,
            action,
            Some(&msg_ref.conversation_id),
            self.daemon_writes_enabled,
        );

        if req.dry_run {
            return Ok(json!({
                "ok": true,
                "dryRun": true,
                "applied": false,
                "archiveVisible": false,
                "saved": save,
                "conversationId": msg_ref.conversation_id,
                "chatName": msg_ref.chat_name,
                "messageId": msg_ref.message_id,
            }));
        }

        auth_check?;
        self.check_write_rate_limit(principal, action)?;

        let tok = self.session.skypetoken()?;
        let status = self.track_upstream(chatsvc::set_saved(&self.transport, &tok, &msg_ref, save))?;
        policy::append_audit_entry(
            principal,
            action,
            Some(&msg_ref.conversation_id),
            Some(&msg_ref.message_id),
            None,
        );

        Ok(json!({
            "ok": true,
            "dryRun": false,
            "applied": true,
            "archiveVisible": false,
            "status": status,
            "saved": save,
            "conversationId": msg_ref.conversation_id,
            "chatName": msg_ref.chat_name,
            "messageId": msg_ref.message_id,
        }))
    }

    pub fn forward_message(
        &self,
        archive: ArchiveSource<'_>,
        principal: &Principal,
        req: ForwardMessageRequest,
    ) -> Result<Value, LiveError> {
        policy::authorize_principal(principal, Action::ForwardMessage, None)?;
        if !principal.is_cli && !principal.has_scope(Scope::Read) {
            return Err(LiveError::forbidden(
                "scope_required",
                format!(
                    "Token '{}' lacks required scope 'read' to read archived messages for forwarding.",
                    principal.name
                ),
            )
            .with_details(json!({ "requiredScope": "read" })));
        }
        if !principal.is_cli && req.format == BodyFormat::Html {
            return Err(LiveError::bad(
                "Raw HTML mode is CLI-only; use plain or markdown over HTTP.",
            ));
        }

        let scope = resolve_scope(principal);
        let (src, dest, display_name) = archive.with_conn(|conn| {
            let src_msg = resolve::resolve_message_scoped(conn, req.from_chat.as_deref(), &req.message_id, scope)?;
            let (dest_chat, _) = resolve::resolve_chat_scoped(conn, &req.to_chat, Strictness::Strict, scope)?;
            let disp = resolve::me_display_name(conn);
            Ok((src_msg, dest_chat, disp))
        })?;

        policy::authorize_principal(principal, Action::ForwardMessage, Some(&dest.id))?;
        if !principal.is_cli && !principal.allows_conversation(&src.conversation_id) {
            return Err(LiveError::forbidden(
                "chat_not_allowed",
                format!(
                    "Source conversation {:?} is not in token '{}' allowlist.",
                    src.conversation_id, principal.name
                ),
            ));
        }
        if !src.archived {
            return Err(LiveError::not_found(format!(
                "Message '{}' not found in archive; run `teams sync` first.",
                req.message_id
            )));
        }
        let auth_check = policy::authorize(
            principal,
            Action::ForwardMessage,
            Some(&dest.id),
            self.daemon_writes_enabled,
        );

        let forward_spec = ForwardSpec {
            message_id: src.message_id.clone(),
            conversation_id: src.conversation_id.clone(),
            chat_kind: src.chat_kind.clone(),
            sender_mri: src.sender_mri.clone(),
            sender_name: src.sender_name.clone(),
            arrived_at_ms: src.arrived_at,
            content_html: src.content_html.clone(),
            text: src.text.clone(),
            message_type: src.message_type.clone(),
        };

        let composed = compose::compose_message(&ComposeInput {
            text: req.comment.clone(),
            format: req.format,
            display_name,
            subject: None,
            importance: Importance::Normal,
            mentions: Vec::new(),
            quote: None,
            forward: Some(forward_spec),
            images: Vec::new(),
            files: Vec::new(),
            client_message_id: None,
            idempotency_key: req.idempotency_key.clone(),
        })?;

        if req.dry_run {
            return Ok(json!({
                "ok": true,
                "dryRun": true,
                "forwarded": false,
                "archiveVisible": false,
                "fromConversationId": src.conversation_id,
                "fromMessageId": src.message_id,
                "toConversationId": dest.id,
                "toChatName": dest.name,
                "comment": req.comment,
                "content": composed.content_html,
                "clientMessageId": composed.client_message_id,
            }));
        }

        auth_check?;
        self.check_write_rate_limit(principal, Action::ForwardMessage)?;

        let tok = self.session.skypetoken()?;
        let mut payload = composed.payload;
        if payload["imdisplayname"].as_str().unwrap_or("").is_empty() && !tok.display_name.is_empty() {
            payload["imdisplayname"] = Value::String(tok.display_name.clone());
        }
        let (status, new_msg_id) =
            self.track_upstream(chatsvc::send_message(&self.transport, &tok, &dest.id, None, &payload))?;

        let hash = policy::content_sha256_hex(&composed.content_html);
        policy::append_audit_entry(
            principal,
            Action::ForwardMessage,
            Some(&dest.id),
            new_msg_id.as_deref(),
            Some(&hash),
        );

        Ok(json!({
            "ok": true,
            "dryRun": false,
            "forwarded": true,
            "archiveVisible": false,
            "status": status,
            "messageId": new_msg_id,
            "fromConversationId": src.conversation_id,
            "fromMessageId": src.message_id,
            "toConversationId": dest.id,
            "toChatName": dest.name,
            "clientMessageId": composed.client_message_id,
        }))
    }
}
