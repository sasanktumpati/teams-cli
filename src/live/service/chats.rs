use crate::live::clients::chatsvc;
use crate::live::error::LiveError;
use crate::live::policy::{self, Action, Principal};
use crate::live::service::{resolve_scope, ArchiveSource, LiveService};
use crate::query::resolve::{self, Strictness};
use crate::timeutil;
use serde_json::{json, Value};

#[derive(Debug, Clone)]
pub struct CreateChatRequest {
    pub members: Vec<String>,
    pub topic: Option<String>,
    pub dry_run: bool,
}

#[derive(Debug, Clone)]
pub struct RenameChatRequest {
    pub chat: String,
    pub topic: String,
    pub dry_run: bool,
}

#[derive(Debug, Clone)]
pub struct MemberChangeRequest {
    pub chat: String,
    pub member: String,
    pub add: bool,
    pub dry_run: bool,
}

#[derive(Debug, Clone)]
pub struct ChatStateRequest {
    pub chat: String,
    pub dry_run: bool,
}

impl LiveService {
    pub fn create_chat(
        &self,
        archive: ArchiveSource<'_>,
        principal: &Principal,
        req: CreateChatRequest,
    ) -> Result<Value, LiveError> {
        policy::authorize_principal(principal, Action::CreateChat, None)?;
        if req.members.is_empty() {
            return Err(LiveError::bad("Provide at least one person to start a chat with."));
        }
        let scope = resolve_scope(principal);
        let (resolved_members, me) = archive.with_conn(|conn| {
            let mut people = Vec::with_capacity(req.members.len());
            for m in &req.members {
                let (p, _) = resolve::resolve_person_scoped(conn, m, Strictness::Strict, scope)?;
                if !people
                    .iter()
                    .any(|x: &resolve::ResolvedPerson| crate::identity::same_person(Some(&x.mri), Some(&p.mri)))
                {
                    people.push(p);
                }
            }
            Ok((people, resolve::me_mri(conn)))
        })?;

        let auth_check = policy::authorize(principal, Action::CreateChat, None, self.daemon_writes_enabled);

        let mut all_mris: Vec<String> = resolved_members.iter().map(|p| p.mri.clone()).collect();
        let mut me_included = false;
        if let Some(ref me_id) = me {
            if !all_mris.contains(me_id) {
                all_mris.insert(0, me_id.clone());
            }
            me_included = true;
        }

        if req.dry_run {
            return Ok(json!({
                "ok": true,
                "dryRun": true,
                "created": false,
                "archiveVisible": false,
                "topic": req.topic,
                "members": resolved_members.iter().map(|p| json!({"mri": p.mri, "name": p.name, "email": p.email})).collect::<Vec<_>>(),
            }));
        }

        auth_check?;
        self.check_write_rate_limit(principal, Action::CreateChat)?;

        let tok = self.session.skypetoken()?;
        if !me_included && !tok.skype_id.is_empty() {
            let me_mri = if tok.skype_id.starts_with("8:") {
                tok.skype_id.clone()
            } else if tok.skype_id.contains(':') {
                format!("8:{}", tok.skype_id)
            } else {
                format!("8:orgid:{}", tok.skype_id)
            };
            if !all_mris.contains(&me_mri) {
                all_mris.insert(0, me_mri);
            }
        }

        let (status, conv_id) = self.track_upstream(chatsvc::create_thread(
            &self.transport,
            &tok,
            &all_mris,
            req.topic.as_deref(),
        ))?;
        policy::append_audit_entry(principal, Action::CreateChat, conv_id.as_deref(), None, None);

        Ok(json!({
            "ok": true,
            "dryRun": false,
            "created": true,
            "archiveVisible": false,
            "status": status,
            "conversationId": conv_id,
            "topic": req.topic,
            "members": resolved_members.iter().map(|p| json!({"mri": p.mri, "name": p.name})).collect::<Vec<_>>(),
        }))
    }

    pub fn rename_chat(
        &self,
        archive: ArchiveSource<'_>,
        principal: &Principal,
        req: RenameChatRequest,
    ) -> Result<Value, LiveError> {
        policy::authorize_principal(principal, Action::RenameChat, None)?;
        let topic = req.topic.trim();
        if topic.is_empty() {
            return Err(LiveError::bad("Chat topic cannot be empty."));
        }
        let scope = resolve_scope(principal);
        let chat = archive
            .with_conn(|conn| Ok(resolve::resolve_chat_scoped(conn, &req.chat, Strictness::Strict, scope)?.0))?;
        policy::authorize_principal(principal, Action::RenameChat, Some(&chat.id))?;
        let auth_check = policy::authorize(
            principal,
            Action::RenameChat,
            Some(&chat.id),
            self.daemon_writes_enabled,
        );

        if req.dry_run {
            return Ok(json!({
                "ok": true,
                "dryRun": true,
                "renamed": false,
                "archiveVisible": false,
                "conversationId": chat.id,
                "chatName": chat.name,
                "topic": topic,
            }));
        }

        auth_check?;
        self.check_write_rate_limit(principal, Action::RenameChat)?;

        let tok = self.session.skypetoken()?;
        let status = self.track_upstream(chatsvc::rename_thread(&self.transport, &tok, &chat.id, topic))?;
        policy::append_audit_entry(principal, Action::RenameChat, Some(&chat.id), None, None);

        Ok(json!({
            "ok": true,
            "dryRun": false,
            "renamed": true,
            "archiveVisible": false,
            "status": status,
            "conversationId": chat.id,
            "previousName": chat.name,
            "topic": topic,
        }))
    }

    pub fn change_member(
        &self,
        archive: ArchiveSource<'_>,
        principal: &Principal,
        req: MemberChangeRequest,
    ) -> Result<Value, LiveError> {
        let action = if req.add {
            Action::AddMember
        } else {
            Action::RemoveMember
        };
        policy::authorize_principal(principal, action, None)?;
        let scope = resolve_scope(principal);
        let (chat, person) = archive.with_conn(|conn| {
            let (c, _) = resolve::resolve_chat_scoped(conn, &req.chat, Strictness::Strict, scope)?;
            policy::authorize_principal(principal, action, Some(&c.id))?;
            let (p, _) = resolve::resolve_person_scoped(conn, &req.member, Strictness::Strict, scope)?;
            Ok((c, p))
        })?;
        let auth_check = policy::authorize(principal, action, Some(&chat.id), self.daemon_writes_enabled);

        if req.dry_run {
            return Ok(json!({
                "ok": true,
                "dryRun": true,
                "applied": false,
                "archiveVisible": false,
                "action": if req.add { "add-member" } else { "remove-member" },
                "conversationId": chat.id,
                "chatName": chat.name,
                "member": {"mri": person.mri, "name": person.name, "email": person.email},
            }));
        }

        auth_check?;
        self.check_write_rate_limit(principal, action)?;

        let tok = self.session.skypetoken()?;
        let status = self.track_upstream(if req.add {
            chatsvc::add_member(&self.transport, &tok, &chat.id, &person.mri)
        } else {
            chatsvc::remove_member(&self.transport, &tok, &chat.id, &person.mri)
        })?;
        policy::append_audit_entry(principal, action, Some(&chat.id), None, None);

        Ok(json!({
            "ok": true,
            "dryRun": false,
            "applied": true,
            "archiveVisible": false,
            "status": status,
            "action": if req.add { "add-member" } else { "remove-member" },
            "conversationId": chat.id,
            "chatName": chat.name,
            "member": {"mri": person.mri, "name": person.name},
        }))
    }

    pub fn set_read_state(
        &self,
        archive: ArchiveSource<'_>,
        principal: &Principal,
        req: ChatStateRequest,
        mark_read: bool,
    ) -> Result<Value, LiveError> {
        let action = if mark_read {
            Action::MarkRead
        } else {
            Action::MarkUnread
        };
        policy::authorize_principal(principal, action, None)?;
        let scope = resolve_scope(principal);
        let (chat, latest_msg_id) = archive.with_conn(|conn| {
            let (c, _) = resolve::resolve_chat_scoped(conn, &req.chat, Strictness::Strict, scope)?;
            policy::authorize_principal(principal, action, Some(&c.id))?;
            let latest: Option<String> = conn.and_then(|db| {
                db.query_row(
                    "SELECT message_id FROM messages WHERE conversation_id = ? ORDER BY arrived_at DESC LIMIT 1",
                    [&c.id],
                    |r| r.get::<_, String>(0),
                )
                .ok()
            });
            Ok((c, latest))
        })?;

        let auth_check = policy::authorize(principal, action, Some(&chat.id), self.daemon_writes_enabled);

        let now = timeutil::now_ms();
        let horizon = if mark_read {
            let msg_ts = latest_msg_id
                .as_deref()
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(now);
            format!("{msg_ts};{now};{msg_ts}")
        } else {
            let back = latest_msg_id
                .as_deref()
                .and_then(|s| s.parse::<i64>().ok())
                .map(|t| t.saturating_sub(1))
                .unwrap_or(0);
            format!("{back};{now};{back}")
        };

        if req.dry_run {
            return Ok(json!({
                "ok": true,
                "dryRun": true,
                "applied": false,
                "archiveVisible": false,
                "read": mark_read,
                "conversationId": chat.id,
                "chatName": chat.name,
                "consumptionHorizon": horizon,
            }));
        }

        auth_check?;
        self.check_write_rate_limit(principal, action)?;

        let tok = self.session.skypetoken()?;
        let status = self.track_upstream(chatsvc::set_consumption_horizon(
            &self.transport,
            &tok,
            &chat.id,
            &horizon,
        ))?;
        policy::append_audit_entry(principal, action, Some(&chat.id), None, None);

        Ok(json!({
            "ok": true,
            "dryRun": false,
            "applied": true,
            "archiveVisible": false,
            "status": status,
            "read": mark_read,
            "conversationId": chat.id,
            "chatName": chat.name,
            "consumptionHorizon": horizon,
        }))
    }

    pub fn set_mute_state(
        &self,
        archive: ArchiveSource<'_>,
        principal: &Principal,
        req: ChatStateRequest,
        muted: bool,
    ) -> Result<Value, LiveError> {
        let action = if muted { Action::MuteChat } else { Action::UnmuteChat };
        policy::authorize_principal(principal, action, None)?;
        let scope = resolve_scope(principal);
        let chat = archive
            .with_conn(|conn| Ok(resolve::resolve_chat_scoped(conn, &req.chat, Strictness::Strict, scope)?.0))?;
        policy::authorize_principal(principal, action, Some(&chat.id))?;
        let auth_check = policy::authorize(principal, action, Some(&chat.id), self.daemon_writes_enabled);

        if req.dry_run {
            return Ok(json!({
                "ok": true,
                "dryRun": true,
                "applied": false,
                "archiveVisible": false,
                "muted": muted,
                "conversationId": chat.id,
                "chatName": chat.name,
            }));
        }

        auth_check?;
        self.check_write_rate_limit(principal, action)?;

        let tok = self.session.skypetoken()?;
        let status = self.track_upstream(chatsvc::set_alerts_muted(&self.transport, &tok, &chat.id, muted))?;
        policy::append_audit_entry(principal, action, Some(&chat.id), None, None);

        Ok(json!({
            "ok": true,
            "dryRun": false,
            "applied": true,
            "archiveVisible": false,
            "status": status,
            "muted": muted,
            "conversationId": chat.id,
            "chatName": chat.name,
        }))
    }
}
