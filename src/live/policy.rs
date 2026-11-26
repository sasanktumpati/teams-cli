use crate::formatters::Quoted;
use crate::json_codec;
use crate::live::error::LiveError;
use crate::paths;
use crate::timeutil;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const ENABLE_WRITES_ENV: &str = "TEAMS_CLI_ENABLE_WRITES";
pub const ALLOWLIST_ENV: &str = "TEAMS_CLI_WRITE_ALLOWLIST";
pub const DEFAULT_WRITES_PER_MINUTE: usize = 30;
const AUDIT_LOG_MAX_BYTES: u64 = 5 * 1024 * 1024;
const AUDIT_LOG_BACKUPS: usize = 3;

static AUDIT_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Scope {
    #[serde(rename = "read")]
    Read,
    #[serde(rename = "sync")]
    Sync,
    #[serde(rename = "presence:read")]
    PresenceRead,
    #[serde(rename = "presence:write")]
    PresenceWrite,
    #[serde(rename = "react")]
    React,
    #[serde(rename = "mark-read")]
    MarkRead,
    #[serde(rename = "send")]
    Send,
    #[serde(rename = "modify")]
    Modify,
    #[serde(rename = "chat:manage")]
    ChatManage,
}

impl Scope {
    pub const ALL: &'static [Scope] = &[
        Scope::Read,
        Scope::Sync,
        Scope::PresenceRead,
        Scope::PresenceWrite,
        Scope::React,
        Scope::MarkRead,
        Scope::Send,
        Scope::Modify,
        Scope::ChatManage,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Scope::Read => "read",
            Scope::Sync => "sync",
            Scope::PresenceRead => "presence:read",
            Scope::PresenceWrite => "presence:write",
            Scope::React => "react",
            Scope::MarkRead => "mark-read",
            Scope::Send => "send",
            Scope::Modify => "modify",
            Scope::ChatManage => "chat:manage",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "read" => Ok(Scope::Read),
            "sync" => Ok(Scope::Sync),
            "presence:read" => Ok(Scope::PresenceRead),
            "presence:write" => Ok(Scope::PresenceWrite),
            "react" => Ok(Scope::React),
            "mark-read" => Ok(Scope::MarkRead),
            "send" => Ok(Scope::Send),
            "modify" => Ok(Scope::Modify),
            "chat:manage" => Ok(Scope::ChatManage),
            other => {
                let valid: Vec<&str> = Self::ALL.iter().map(Scope::as_str).collect();
                Err(format!("Unknown scope '{}'. Valid scopes: {}", other, valid.join(", ")))
            }
        }
    }

    pub fn is_live_write(&self) -> bool {
        matches!(
            self,
            Scope::PresenceWrite | Scope::React | Scope::MarkRead | Scope::Send | Scope::Modify | Scope::ChatManage
        )
    }

    pub fn is_chat_targeted(&self) -> bool {
        matches!(
            self,
            Scope::React | Scope::MarkRead | Scope::Send | Scope::Modify | Scope::ChatManage
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    SendMessage,
    EditMessage,
    DeleteMessage,
    React,
    Unreact,
    Pin,
    Unpin,
    Save,
    Unsave,
    ForwardMessage,
    CreateChat,
    RenameChat,
    AddMember,
    RemoveMember,
    MarkRead,
    MarkUnread,
    MuteChat,
    UnmuteChat,
    GetPresence,
    SetPresence,
    SetStatusMessage,
    ClearStatusMessage,
}

impl Action {
    pub fn required_scope(&self) -> Scope {
        match self {
            Action::SendMessage | Action::ForwardMessage => Scope::Send,
            Action::EditMessage | Action::DeleteMessage => Scope::Modify,
            Action::React
            | Action::Unreact
            | Action::Pin
            | Action::Unpin
            | Action::Save
            | Action::Unsave
            | Action::MuteChat
            | Action::UnmuteChat => Scope::React,
            Action::MarkRead | Action::MarkUnread => Scope::MarkRead,
            Action::CreateChat | Action::RenameChat | Action::AddMember | Action::RemoveMember => Scope::ChatManage,
            Action::GetPresence => Scope::PresenceRead,
            Action::SetPresence | Action::SetStatusMessage | Action::ClearStatusMessage => Scope::PresenceWrite,
        }
    }

    pub fn requires_cli_confirm(&self) -> bool {
        matches!(self.required_scope(), Scope::Send | Scope::Modify | Scope::ChatManage)
    }

    pub fn is_write(&self) -> bool {
        self.required_scope().is_live_write()
    }

    pub fn audit_name(&self) -> &'static str {
        match self {
            Action::SendMessage => "message.send",
            Action::EditMessage => "message.edit",
            Action::DeleteMessage => "message.delete",
            Action::React => "message.react",
            Action::Unreact => "message.unreact",
            Action::Pin => "message.pin",
            Action::Unpin => "message.unpin",
            Action::Save => "message.save",
            Action::Unsave => "message.unsave",
            Action::ForwardMessage => "message.forward",
            Action::CreateChat => "chat.create",
            Action::RenameChat => "chat.rename",
            Action::AddMember => "chat.add_member",
            Action::RemoveMember => "chat.remove_member",
            Action::MarkRead => "chat.read",
            Action::MarkUnread => "chat.unread",
            Action::MuteChat => "chat.mute",
            Action::UnmuteChat => "chat.unmute",
            Action::GetPresence => "presence.get",
            Action::SetPresence => "presence.set",
            Action::SetStatusMessage => "presence.message.set",
            Action::ClearStatusMessage => "presence.message.clear",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Principal {
    pub id: String,
    pub name: String,
    pub is_cli: bool,
    pub scopes: Vec<Scope>,
    pub allow_chats: Option<Vec<String>>,
}

impl Principal {
    pub fn cli() -> Self {
        Self {
            id: "cli".to_string(),
            name: "cli".to_string(),
            is_cli: true,
            scopes: Scope::ALL.to_vec(),
            allow_chats: None,
        }
    }

    pub fn has_scope(&self, scope: Scope) -> bool {
        self.scopes.contains(&scope)
    }

    pub fn allows_conversation(&self, conv_id: &str) -> bool {
        match &self.allow_chats {
            None => true,
            Some(list) => list.iter().any(|c| c == conv_id),
        }
    }
}

pub fn is_cli_writes_enabled() -> bool {
    let val = env::var(ENABLE_WRITES_ENV).unwrap_or_default();
    matches!(val.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

pub fn global_allowlist() -> Option<HashSet<String>> {
    let raw = env::var(ALLOWLIST_ENV).ok()?;
    let items: HashSet<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    if items.is_empty() {
        None
    } else {
        Some(items)
    }
}

pub fn is_conversation_globally_allowed(conv_id: &str) -> bool {
    match global_allowlist() {
        None => true,
        Some(set) => set.contains(conv_id),
    }
}

pub fn authorize_principal(
    principal: &Principal,
    action: Action,
    conversation_id: Option<&str>,
) -> Result<(), LiveError> {
    if principal.is_cli {
        return Ok(());
    }
    let required = action.required_scope();
    if !principal.has_scope(required) {
        return Err(LiveError::forbidden(
            "scope_required",
            format!(
                "Token '{}' lacks required scope '{}'. Create a token with `--scope {}`.",
                principal.name,
                required.as_str(),
                required.as_str()
            ),
        )
        .with_details(json!({ "requiredScope": required.as_str() })));
    }
    if action == Action::CreateChat && principal.allow_chats.is_some() {
        return Err(LiveError::forbidden(
            "chat_not_allowed",
            format!(
                "Token '{}' has a --chats allowlist and cannot create new conversations.",
                principal.name
            ),
        ));
    }
    if let Some(cid) = conversation_id {
        if !principal.allows_conversation(cid) {
            return Err(LiveError::forbidden(
                "chat_not_allowed",
                format!(
                    "Conversation {} is not in token '{}' allowlist.",
                    Quoted(cid),
                    principal.name
                ),
            ));
        }
    }
    Ok(())
}

pub fn authorize(
    principal: &Principal,
    action: Action,
    conversation_id: Option<&str>,
    daemon_writes_enabled: bool,
) -> Result<(), LiveError> {
    authorize_principal(principal, action, conversation_id)?;

    if !principal.is_cli {
        if action.is_write() && !daemon_writes_enabled {
            return Err(LiveError::forbidden(
                "writes_disabled",
                "Daemon live writes are disabled. Reinstall with `teams daemon install --allow-writes` or run with `--allow-writes`.",
            ));
        }
        if action == Action::CreateChat && global_allowlist().is_some() {
            return Err(LiveError::forbidden(
                "chat_not_allowed",
                format!("Creating new conversations is blocked while {ALLOWLIST_ENV} is set."),
            ));
        }
        if let Some(cid) = conversation_id {
            if action.is_write() && !is_conversation_globally_allowed(cid) {
                return Err(LiveError::forbidden(
                    "chat_not_allowed",
                    format!("Conversation {} is not in {ALLOWLIST_ENV}.", Quoted(cid)),
                ));
            }
        }
        return Ok(());
    }

    if action.requires_cli_confirm() && !is_cli_writes_enabled() {
        return Err(LiveError::forbidden(
            "writes_disabled",
            format!("Live writes are restricted. Set {ENABLE_WRITES_ENV}=1 to enable (or use --dry-run to preview)."),
        ));
    }
    if action == Action::CreateChat && global_allowlist().is_some() {
        return Err(LiveError::forbidden(
            "chat_not_allowed",
            format!("Creating new conversations is blocked while {ALLOWLIST_ENV} is set."),
        ));
    }
    if let Some(cid) = conversation_id {
        if action.is_write() && !is_conversation_globally_allowed(cid) {
            return Err(LiveError::forbidden(
                "chat_not_allowed",
                format!("Conversation {} is not in {ALLOWLIST_ENV}.", Quoted(cid)),
            ));
        }
    }
    Ok(())
}

pub struct WriteRateLimiter {
    limit: usize,
    window: Duration,
    buckets: Mutex<HashMap<String, VecDeque<Instant>>>,
}

impl Default for WriteRateLimiter {
    fn default() -> Self {
        Self::new(DEFAULT_WRITES_PER_MINUTE, Duration::from_secs(60))
    }
}

impl WriteRateLimiter {
    pub fn new(limit: usize, window: Duration) -> Self {
        Self {
            limit,
            window,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    pub fn check_and_record(&self, key: &str) -> Result<(), LiveError> {
        let now = Instant::now();
        let mut map = self.buckets.lock().unwrap_or_else(|p| p.into_inner());
        if map.len() > 64 {
            map.retain(|_, q| {
                while let Some(&front) = q.front() {
                    if now.duration_since(front) >= self.window {
                        q.pop_front();
                    } else {
                        break;
                    }
                }
                !q.is_empty()
            });
        }
        let q = map.entry(key.to_string()).or_default();
        while let Some(&front) = q.front() {
            if now.duration_since(front) >= self.window {
                q.pop_front();
            } else {
                break;
            }
        }
        if q.len() >= self.limit {
            let wait = q
                .front()
                .map(|&f| self.window.saturating_sub(now.duration_since(f)).as_secs().max(1))
                .unwrap_or(1);
            return Err(LiveError::rate_limited(wait));
        }
        q.push_back(now);
        Ok(())
    }
}

pub fn content_sha256_hex(content: &str) -> String {
    hex::encode(Sha256::digest(content.as_bytes()))
}

fn rotate_audit_log_if_needed(path: &Path) {
    if fs::metadata(path).map(|m| m.len()).unwrap_or(0) < AUDIT_LOG_MAX_BYTES {
        return;
    }
    for i in (1..=AUDIT_LOG_BACKUPS).rev() {
        let src = if i == 1 {
            path.to_path_buf()
        } else {
            Path::new(&format!("{}.{}", path.display(), i - 1)).to_path_buf()
        };
        let dst = Path::new(&format!("{}.{}", path.display(), i)).to_path_buf();
        if src.exists() {
            let _ = fs::rename(&src, &dst);
        }
    }
}

pub fn append_audit_entry(
    principal: &Principal,
    action: Action,
    conversation_id: Option<&str>,
    message_id: Option<&str>,
    content_hash: Option<&str>,
) {
    let path = paths::write_audit_log_path();
    let entry = json!({
        "time": timeutil::iso(Some(timeutil::now_ms())),
        "actorId": principal.id,
        "actorName": principal.name,
        "action": action.audit_name(),
        "conversationId": conversation_id,
        "messageId": message_id,
        "contentSha256": content_hash,
    });
    let line = format!("{}\n", json_codec::to_string(&entry));
    let _g = AUDIT_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let lock_path = path.with_file_name(".write-audit.lock");
    let _flock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)
        .ok()
        .and_then(|f| f.lock().ok().map(|()| f));
    rotate_audit_log_if_needed(&path);
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).mode(0o600).open(&path) {
        let _ = f.write_all(line.as_bytes());
    }
}
