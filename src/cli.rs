use crate::completions;
use crate::daemon;
use crate::doctor;
use crate::formatters::{self as fmt, Quoted};
use crate::ingest::{indexeddb::TeamsCache, sync};
use crate::json_codec;
use crate::launchd;
use crate::live::clients::{ams, sharepoint};
use crate::live::compose::{self, BodyFormat, Importance};
use crate::live::error::LiveError;
use crate::live::fetch;
use crate::live::policy::{self, Principal, Scope};
use crate::live::service::chats::{ChatStateRequest, CreateChatRequest, MemberChangeRequest, RenameChatRequest};
use crate::live::service::messages::{
    EditMessageRequest, ForwardMessageRequest, MediaUpload, MessageActionRequest, ReactMessageRequest,
    SendMessageRequest,
};
use crate::live::service::{ArchiveSource, LiveService};
use crate::live::session;
use crate::paths;
use crate::query::resolve::{self, Strictness};
use crate::query::{api, export, views};
use crate::storage::{archive, backups, migrations};
use crate::timeutil::{iso, short};
use crate::update;
use clap::{Args, CommandFactory, Parser, Subcommand};
use serde_json::{json, Value};
use std::fs;
use std::io::Read;
use std::path::PathBuf;

pub const VERSION: &str = crate::VERSION;
pub const MIN_SYNC_INTERVAL: u64 = 10;

#[derive(Args, Debug, Clone)]
pub struct TimeRangeArgs {
    #[arg(long, help = "Only items at or after this time (e.g. 7d, today, 2026-11-01)")]
    pub since: Option<String>,
    #[arg(long, help = "Only items before this time")]
    pub until: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct WriteConfirmArgs {
    #[arg(long, help = "Preview the request without executing")]
    pub dry_run: bool,
    #[arg(long, help = "Execute the live action (also requires TEAMS_CLI_ENABLE_WRITES=1)")]
    pub yes: bool,
    #[arg(long, help = "Output as JSON")]
    pub json: bool,
}

impl WriteConfirmArgs {
    fn is_dry_run(&self) -> bool {
        self.dry_run || !self.yes
    }
}

#[derive(Args, Debug, Clone)]
pub struct LightWriteArgs {
    #[arg(long, help = "Preview the request without executing")]
    pub dry_run: bool,
    #[arg(long, help = "Output as JSON")]
    pub json: bool,
}

#[derive(Parser)]
#[command(
    name = "teams",
    display_name = "teams-cli",
    version = VERSION,
    about = "Durable local archive, CLI, and scoped HTTP API for Microsoft Teams."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    #[command(about = "What needs attention: unread chats, mentions, awaiting your reply, unread activity")]
    Inbox {
        #[arg(
            long,
            default_value = "7d",
            help = "Activity at or after this time (\"all\" for entire archive)"
        )]
        since: String,
        #[arg(
            long,
            default_value_t = 30,
            value_parser = nonneg_int,
            allow_negative_numbers = true,
            help = "Maximum items per section (0 = all)"
        )]
        limit: usize,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },

    #[command(about = "Full-text search across every archived message")]
    Search {
        #[arg(help = "Search words, \"exact phrase\", OR, NOT, or -exclude")]
        query: Vec<String>,
        #[arg(long, help = "Limit to one chat (name fragment or id)")]
        chat: Option<String>,
        #[arg(
            long,
            help = "Rank by BM25 relevance (subject weighted higher) instead of newest first"
        )]
        relevance: bool,
        #[arg(long, help = "Also match wording later edited out of messages")]
        history: bool,
        #[arg(long, help = "Return only the match count")]
        count: bool,
        #[arg(long, help = "Only messages that have at least one file, image, or link attachment")]
        has_attachments: bool,
        #[command(flatten)]
        time: TimeRangeArgs,
        #[arg(long = "from", help = "Limit to one sender (name, email, or id)")]
        sender: Option<String>,
        #[arg(
            long,
            default_value_t = 30,
            value_parser = nonneg_int,
            allow_negative_numbers = true,
            help = "Maximum results to return (0 = all)"
        )]
        limit: usize,
        #[arg(long, help = "Include system events")]
        events: bool,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },

    #[command(about = "Messages that @mention you, newest first")]
    Mentions {
        #[command(flatten)]
        time: TimeRangeArgs,
        #[arg(long, help = "Limit to one chat (name fragment or id)")]
        chat: Option<String>,
        #[arg(long, help = "Show only mentions marked unread in Teams")]
        unread: bool,
        #[arg(
            long,
            default_value_t = 50,
            value_parser = nonneg_int,
            allow_negative_numbers = true,
            help = "Maximum mentions to return (0 = all)"
        )]
        limit: usize,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },

    #[command(about = "Messages edited or deleted recently, with what they said before")]
    Changes {
        #[arg(
            long,
            default_value = "7d",
            help = "Changes at or after this time (\"all\" for entire archive)"
        )]
        since: String,
        #[arg(long, help = "Changes before this time")]
        until: Option<String>,
        #[arg(long, help = "Limit to one chat (name fragment or id)")]
        chat: Option<String>,
        #[arg(long, conflicts_with = "deleted", help = "Show only edited messages")]
        edited: bool,
        #[arg(long, conflicts_with = "edited", help = "Show only deleted messages")]
        deleted: bool,
        #[arg(
            long,
            default_value_t = 50,
            value_parser = nonneg_int,
            allow_negative_numbers = true,
            help = "Maximum results to return (0 = all)"
        )]
        limit: usize,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },

    #[command(about = "List, export, create, rename, manage members, read/unread, or mute chats")]
    Chat {
        #[command(subcommand)]
        action: ChatAction,
    },

    #[command(
        visible_alias = "msg",
        about = "Read, send, edit, delete, react, pin, save, or forward messages"
    )]
    Message {
        #[command(subcommand)]
        action: MessageAction,
    },

    #[command(about = "List and download shared files, images, and links")]
    File {
        #[command(subcommand)]
        action: FileAction,
    },

    #[command(about = "List people seen in Teams, or inspect one person by name, email, or id")]
    People {
        #[arg(help = "Inspect one person (name fragment, email, or Teams id); omit to list all")]
        person: Vec<String>,
        #[arg(
            long,
            conflicts_with = "person",
            help = "Filter list by name, email, or job title substring"
        )]
        grep: Option<String>,
        #[arg(long, conflicts_with = "person", help = "Include bots when listing people")]
        bots: bool,
        #[arg(
            long,
            value_parser = nonneg_int,
            allow_negative_numbers = true,
            help = "Maximum results to return (default 50 for list, 20 for one person; 0 = all)"
        )]
        limit: Option<usize>,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },

    #[command(about = "Check live presence, set availability, or update your Teams status message")]
    Presence {
        #[command(subcommand)]
        action: Option<PresenceAction>,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },

    #[command(about = "Call history with direction, state, duration and participants")]
    Calls {
        #[command(flatten)]
        time: TimeRangeArgs,
        #[arg(long = "with", help = "Filter to calls involving this person (name, email, or id)")]
        person: Option<String>,
        #[arg(long, help = "Show only missed calls")]
        missed: bool,
        #[arg(
            long,
            default_value_t = 50,
            value_parser = nonneg_int,
            allow_negative_numbers = true,
            help = "Maximum calls to return (0 = all)"
        )]
        limit: usize,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },

    #[command(about = "Calendar meetings Teams cached, newest start first")]
    Meetings {
        #[command(flatten)]
        time: TimeRangeArgs,
        #[arg(long, help = "Filter by subject, organizer, location, or attendee substring")]
        grep: Option<String>,
        #[arg(
            long,
            default_value_t = 50,
            value_parser = nonneg_int,
            allow_negative_numbers = true,
            help = "Maximum meetings to return (0 = all)"
        )]
        limit: usize,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },

    #[command(about = "Copy new messages from the Teams cache into the archive")]
    Sync {
        #[arg(
            long,
            value_hint = clap::ValueHint::DirPath,
            help = "Override the Teams IndexedDB directory path"
        )]
        source: Option<String>,
        #[arg(long, help = "Re-read the Teams cache even if its file fingerprint is unchanged")]
        force: bool,
        #[arg(long, hide = true)]
        notify: bool,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },

    #[command(about = "Archive health, counts and last sync")]
    Status {
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },

    #[command(about = "Diagnose local setup: Teams cache permissions, archive, daemon, API auth, and backups")]
    Doctor {
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },

    #[command(about = "Install, manage, inspect logs, or run the background daemon and local HTTP API")]
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },

    #[command(about = "Archive maintenance: backup, restore, verify, reindex, and migrations")]
    Archive {
        #[command(subcommand)]
        action: ArchiveAction,
    },

    #[command(about = "Manage scoped HTTP API bearer tokens (SHA-256 hashed; secret shown once)")]
    Token {
        #[command(subcommand)]
        action: TokenAction,
    },

    #[command(about = "Check for or install the latest teams-cli release from GitHub")]
    Update {
        #[arg(long, help = "Check if a newer release is available without installing")]
        check: bool,
        #[arg(
            long = "version",
            value_name = "TAG",
            help = "Install a specific release tag (e.g. v0.3.0)"
        )]
        target_version: Option<String>,
        #[arg(long, help = "Reinstall even if already on the latest version")]
        force: bool,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(long, hide = true)]
        background_check: bool,
    },

    #[command(about = "Inspect raw archived records and the live Teams IndexedDB cache")]
    Debug {
        #[command(subcommand)]
        action: DebugAction,
    },

    #[command(
        about = "Print shell completion scripts (bash, zsh, fish, elvish, powershell)",
        after_long_help = "Examples:\n  \
            teams completions zsh  > \"${fpath[1]}/_teams\"\n  \
            teams completions fish > ~/.config/fish/completions/teams.fish\n  \
            teams completions bash > ~/.local/share/bash-completion/completions/teams"
    )]
    Completions {
        #[arg(value_enum, help = "Target shell")]
        shell: clap_complete::Shell,
    },

    #[command(about = "Everything on one page: commands, flags, JSON shapes (for agents and scripts)")]
    Docs,

    #[command(name = "__complete", hide = true)]
    Complete {
        #[arg(long, default_value = "fish")]
        shell: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        words: Vec<String>,
    },
}

#[derive(Subcommand)]
pub enum ChatAction {
    #[command(about = "List chats, channels and meetings by recent activity")]
    List {
        #[arg(long, help = "Filter chats by name fragment or exact conversation id")]
        grep: Option<String>,
        #[arg(
            long,
            value_parser = ["chat", "channel", "meeting", "team", "community", "system"],
            help = "Filter by conversation kind"
        )]
        kind: Option<String>,
        #[arg(long, help = "Include system feeds (notifications, call logs, notes)")]
        all: bool,
        #[arg(long, help = "Filter to chats that include this person (name, email, or id)")]
        member: Option<String>,
        #[arg(
            long,
            default_value_t = 40,
            value_parser = nonneg_int,
            allow_negative_numbers = true,
            help = "Maximum chats to return (0 = all)"
        )]
        limit: usize,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },
    #[command(about = "Export a chat as Markdown (whole history by default)")]
    Export {
        #[arg(help = "Chat name fragment or conversation id")]
        chat: String,
        #[command(flatten)]
        time: TimeRangeArgs,
        #[arg(long = "from", help = "Only messages from this sender")]
        sender: Option<String>,
        #[arg(long, help = "Export a single channel thread root message id")]
        thread: Option<String>,
        #[arg(long, help = "Include system events")]
        events: bool,
        #[arg(long, help = "Include earlier versions under edited messages")]
        history: bool,
        #[arg(
            long,
            value_hint = clap::ValueHint::FilePath,
            help = "Write Markdown to this file (mode 0600) instead of stdout"
        )]
        out: Option<String>,
    },
    #[command(about = "[LIVE] Start a new 1:1 or group chat")]
    New {
        #[arg(required = true, help = "People to include (name, email, or MRI)")]
        members: Vec<String>,
        #[arg(long, help = "Optional group chat topic")]
        topic: Option<String>,
        #[command(flatten)]
        write: WriteConfirmArgs,
    },
    #[command(about = "[LIVE] Rename a group chat topic")]
    Rename {
        #[arg(help = "Chat name or conversation id")]
        chat: String,
        #[arg(help = "New chat topic")]
        topic: String,
        #[command(flatten)]
        write: WriteConfirmArgs,
    },
    #[command(about = "[LIVE] Add a member to a group chat")]
    AddMember {
        #[arg(help = "Chat name or conversation id")]
        chat: String,
        #[arg(help = "Person to add (name, email, or MRI)")]
        member: String,
        #[command(flatten)]
        write: WriteConfirmArgs,
    },
    #[command(about = "[LIVE] Remove a member from a group chat")]
    RemoveMember {
        #[arg(help = "Chat name or conversation id")]
        chat: String,
        #[arg(help = "Person to remove (name, email, or MRI)")]
        member: String,
        #[command(flatten)]
        write: WriteConfirmArgs,
    },
    #[command(about = "[LIVE] Mark a chat as read in Teams")]
    Read {
        #[arg(help = "Chat name or conversation id")]
        chat: String,
        #[command(flatten)]
        flags: LightWriteArgs,
    },
    #[command(about = "[LIVE] Mark a chat as unread in Teams")]
    Unread {
        #[arg(help = "Chat name or conversation id")]
        chat: String,
        #[command(flatten)]
        flags: LightWriteArgs,
    },
    #[command(about = "[LIVE] Mute notifications for a chat")]
    Mute {
        #[arg(help = "Chat name or conversation id")]
        chat: String,
        #[command(flatten)]
        flags: LightWriteArgs,
    },
    #[command(about = "[LIVE] Unmute notifications for a chat")]
    Unmute {
        #[arg(help = "Chat name or conversation id")]
        chat: String,
        #[command(flatten)]
        flags: LightWriteArgs,
    },
}

#[derive(Subcommand)]
pub enum MessageAction {
    #[command(about = "Read archived messages in a chat, or around one message")]
    List {
        #[arg(help = "Chat name fragment or conversation id")]
        chat: Option<String>,
        #[arg(long, help = "Message id (or chatId/msgId) to inspect with surrounding context")]
        around: Option<String>,
        #[arg(
            long,
            default_value_t = 10,
            value_parser = nonneg_int,
            allow_negative_numbers = true,
            help = "Number of messages before and after `--around`"
        )]
        context: usize,
        #[arg(long, help = "Filter to a single channel thread root message id")]
        thread: Option<String>,
        #[arg(long, help = "Include earlier versions of edited messages")]
        history: bool,
        #[command(flatten)]
        time: TimeRangeArgs,
        #[arg(long = "from", help = "Only messages sent by this person (name, email, or id)")]
        sender: Option<String>,
        #[arg(
            long,
            default_value_t = 100,
            value_parser = nonneg_int,
            allow_negative_numbers = true,
            help = "Maximum messages to return (0 = all)"
        )]
        limit: usize,
        #[arg(long, help = "Include system events (joins, renames, calls, recordings)")]
        events: bool,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },
    #[command(about = "Messages you saved (bookmarked), most recently saved first")]
    Saved {
        #[arg(long, help = "Include messages you later unsaved")]
        include_unsaved: bool,
        #[arg(
            long,
            default_value_t = 50,
            value_parser = nonneg_int,
            allow_negative_numbers = true,
            help = "Maximum saved messages to return (0 = all)"
        )]
        limit: usize,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },
    #[command(about = "Messages pinned in chats and channels")]
    Pinned {
        #[arg(long, help = "Limit to one chat (name fragment or id)")]
        chat: Option<String>,
        #[arg(
            long,
            default_value_t = 50,
            value_parser = nonneg_int,
            allow_negative_numbers = true,
            help = "Maximum pinned messages to return (0 = all)"
        )]
        limit: usize,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },
    #[command(about = "[LIVE] Send a message with optional Markdown, @mentions, quote, images, or files")]
    Send {
        #[arg(help = "Target conversation id or unambiguous chat name")]
        target: String,
        #[arg(help = "Message body text")]
        message: Vec<String>,
        #[arg(long, help = "Read message body from stdin")]
        stdin: bool,
        #[arg(long, help = "Reply to a numeric channel thread root message id")]
        reply_to: Option<String>,
        #[arg(long, help = "Quote an existing message (msgId or chatId/msgId)")]
        quote: Option<String>,
        #[arg(long, help = "Optional post subject line")]
        subject: Option<String>,
        #[arg(long, conflicts_with = "urgent", help = "Mark message as important")]
        important: bool,
        #[arg(long, conflicts_with = "important", help = "Mark message as urgent")]
        urgent: bool,
        #[arg(long, conflicts_with = "html", help = "Render body from Markdown to Teams HTML")]
        markdown: bool,
        #[arg(
            long,
            conflicts_with = "markdown",
            help = "Send body as raw HTML instead of escaping"
        )]
        html: bool,
        #[arg(long, help = "Real @mention for a person (repeatable; name, email, or MRI)")]
        mention: Vec<String>,
        #[arg(
            long,
            value_hint = clap::ValueHint::FilePath,
            help = "Upload and embed an inline image/GIF (repeatable)"
        )]
        image: Vec<PathBuf>,
        #[arg(
            long,
            value_hint = clap::ValueHint::FilePath,
            help = "Upload to OneDrive/SharePoint and attach a file (repeatable)"
        )]
        file: Vec<PathBuf>,
        #[arg(long, help = "Explicit numeric client message id (1–32 digits)")]
        client_message_id: Option<String>,
        #[arg(long, help = "Idempotency key hashed to a stable client message id")]
        idempotency_key: Option<String>,
        #[command(flatten)]
        write: WriteConfirmArgs,
    },
    #[command(about = "[LIVE] Edit one of your sent messages")]
    Edit {
        #[arg(help = "Message id (msgId or chatId/msgId)")]
        message: String,
        #[arg(help = "Replacement message text")]
        text: Vec<String>,
        #[arg(long, help = "Chat name or id (optional when msgId is unique or chatId/msgId)")]
        chat: Option<String>,
        #[arg(long, help = "Read replacement body from stdin")]
        stdin: bool,
        #[arg(long, conflicts_with = "html", help = "Render body from Markdown")]
        markdown: bool,
        #[arg(long, conflicts_with = "markdown", help = "Send body as raw HTML")]
        html: bool,
        #[command(flatten)]
        write: WriteConfirmArgs,
    },
    #[command(about = "[LIVE] Delete one of your sent messages")]
    Delete {
        #[arg(help = "Message id (msgId or chatId/msgId)")]
        message: String,
        #[arg(long, help = "Chat name or id")]
        chat: Option<String>,
        #[command(flatten)]
        write: WriteConfirmArgs,
    },
    #[command(about = "[LIVE] Add a reaction to a message")]
    React {
        #[arg(help = "Message id (msgId or chatId/msgId)")]
        message: String,
        #[arg(help = "Emoji or Teams key (e.g. 👍, ❤️, like, heart, 1f389_partypopper)")]
        reaction: String,
        #[arg(long, help = "Chat name or id")]
        chat: Option<String>,
        #[command(flatten)]
        flags: LightWriteArgs,
    },
    #[command(about = "[LIVE] Remove a reaction from a message")]
    Unreact {
        #[arg(help = "Message id (msgId or chatId/msgId)")]
        message: String,
        #[arg(help = "Emoji or Teams key to remove")]
        reaction: String,
        #[arg(long, help = "Chat name or id")]
        chat: Option<String>,
        #[command(flatten)]
        flags: LightWriteArgs,
    },
    #[command(about = "[LIVE] Pin a message in its chat or channel")]
    Pin {
        #[arg(help = "Message id (msgId or chatId/msgId)")]
        message: String,
        #[arg(long, help = "Chat name or id")]
        chat: Option<String>,
        #[command(flatten)]
        flags: LightWriteArgs,
    },
    #[command(about = "[LIVE] Unpin a message")]
    Unpin {
        #[arg(help = "Message id (msgId or chatId/msgId)")]
        message: String,
        #[arg(long, help = "Chat name or id")]
        chat: Option<String>,
        #[command(flatten)]
        flags: LightWriteArgs,
    },
    #[command(about = "[LIVE] Save (bookmark) a message")]
    Save {
        #[arg(help = "Message id (msgId or chatId/msgId)")]
        message: String,
        #[arg(long, help = "Chat name or id")]
        chat: Option<String>,
        #[command(flatten)]
        flags: LightWriteArgs,
    },
    #[command(about = "[LIVE] Unsave a bookmarked message")]
    Unsave {
        #[arg(help = "Message id (msgId or chatId/msgId)")]
        message: String,
        #[arg(long, help = "Chat name or id")]
        chat: Option<String>,
        #[command(flatten)]
        flags: LightWriteArgs,
    },
    #[command(about = "[LIVE] Forward an existing message to another chat")]
    Forward {
        #[arg(help = "Source message id (msgId or chatId/msgId)")]
        message: String,
        #[arg(help = "Destination chat name or conversation id")]
        to_chat: String,
        #[arg(long, help = "Source chat name or id")]
        from_chat: Option<String>,
        #[arg(long, help = "Optional comment above the forwarded message")]
        comment: Option<String>,
        #[arg(long, help = "Render comment from Markdown")]
        markdown: bool,
        #[arg(long, help = "Idempotency key hashed to a stable client message id")]
        idempotency_key: Option<String>,
        #[command(flatten)]
        write: WriteConfirmArgs,
    },
}

#[derive(Subcommand)]
pub enum FileAction {
    #[command(about = "List files, images and links shared in messages")]
    List {
        #[arg(help = "Filter by file name or URL substring")]
        query: Vec<String>,
        #[arg(long, help = "Limit to one chat (name fragment or id)")]
        chat: Option<String>,
        #[arg(long, value_parser = ["file", "image", "link"], help = "Filter by attachment kind")]
        kind: Option<String>,
        #[arg(long = "type", help = "Filter files by extension (e.g. pdf, xlsx, zip)")]
        file_type: Option<String>,
        #[command(flatten)]
        time: TimeRangeArgs,
        #[arg(long = "from", help = "Limit to one sender (name, email, or id)")]
        sender: Option<String>,
        #[arg(
            long,
            default_value_t = 50,
            value_parser = nonneg_int,
            allow_negative_numbers = true,
            help = "Maximum files to return (0 = all)"
        )]
        limit: usize,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },
    #[command(about = "Download a shared file or pasted image on demand")]
    Fetch {
        #[arg(help = "Attachment URL, message id, or filename query")]
        target: Vec<String>,
        #[arg(long, help = "Message id (or chatId/msgId) owning the attachment")]
        message: Option<String>,
        #[arg(long, help = "1-based attachment index when a message or query has multiple matches")]
        index: Option<usize>,
        #[arg(
            long,
            value_hint = clap::ValueHint::AnyPath,
            help = "Destination file or directory path"
        )]
        out: Option<String>,
        #[arg(long, help = "Download from Microsoft even if a local OneDrive copy exists")]
        force_download: bool,
        #[arg(
            long,
            default_value_t = fetch::DEFAULT_MAX_BYTES,
            help = "Maximum allowed download size in bytes (default 2 GiB, 0 = unlimited)"
        )]
        max_bytes: u64,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum PresenceAction {
    #[command(about = "[LIVE] Get your own live presence or look up people")]
    Get {
        #[arg(help = "Optional people (name, email, or MRI); omit for your own presence")]
        person: Vec<String>,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },
    #[command(about = "[LIVE] Set your availability (available, busy, dnd, brb, away, offline, reset)")]
    Set {
        #[arg(
            value_parser = ["available", "busy", "dnd", "brb", "away", "offline", "reset"],
            help = "Target availability"
        )]
        availability: String,
        #[arg(long = "for", help = "Optional duration or expiry (e.g. 30m, 2h, 1d)")]
        expires: Option<String>,
        #[command(flatten)]
        flags: LightWriteArgs,
    },
    #[command(about = "[LIVE] Set or clear your Teams status message")]
    Message {
        #[command(subcommand)]
        action: PresenceMessageAction,
    },
}

#[derive(Subcommand)]
pub enum PresenceMessageAction {
    #[command(about = "[LIVE] Publish a status message")]
    Set {
        #[arg(required = true, help = "Status message text")]
        message: Vec<String>,
        #[arg(long = "for", help = "Optional expiry (e.g. 1h, 4h, 1d)")]
        expires: Option<String>,
        #[command(flatten)]
        flags: LightWriteArgs,
    },
    #[command(about = "[LIVE] Clear your status message")]
    Clear {
        #[command(flatten)]
        flags: LightWriteArgs,
    },
}

#[derive(Subcommand)]
pub enum TokenAction {
    #[command(about = "Create a scoped named bearer token (stored SHA-256 hashed; secret printed once)")]
    Create {
        #[arg(help = "Human-readable label for the token (e.g. executor, ci, bot)")]
        name: String,
        #[arg(
            long,
            value_delimiter = ',',
            help = "Comma-separated scopes (default: read). Options: read, sync, presence:read, presence:write, react, mark-read, send, modify, chat:manage"
        )]
        scope: Vec<String>,
        #[arg(
            long,
            value_delimiter = ',',
            help = "Restrict live actions for this token to these chats (resolved to exact conversation ids)"
        )]
        chats: Vec<String>,
        #[arg(long, help = "Optional expiry (e.g. 12h, 7d, 30d, 1y, 2027-01-01, or never)")]
        expires: Option<String>,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },
    #[command(about = "List named bearer tokens (metadata and scopes only; secrets are never shown)")]
    List {
        #[arg(long, help = "Also show expired tokens")]
        include_expired: bool,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },
    #[command(about = "Revoke a named bearer token immediately by name or id")]
    Revoke {
        #[arg(help = "Token name or 8-character id")]
        target: String,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },
    #[command(about = "Remove all expired named tokens from api.tokens.json")]
    Prune {
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum DaemonAction {
    #[command(about = "Install and start the launchd background daemon")]
    Install {
        #[arg(
            long,
            default_value_t = paths::DEFAULT_SYNC_INTERVAL,
            value_parser = min_interval,
            help = "Seconds between lightweight cache refresh checks (minimum 10)"
        )]
        interval: u64,
        #[arg(long, help = "Loopback HTTP port (default 47613 or TEAMS_CLI_PORT)")]
        port: Option<u16>,
        #[arg(
            long,
            help = "Enable scoped live write routes on the HTTP API (runs an interactive Keychain check)"
        )]
        allow_writes: bool,
    },
    #[command(about = "Stop and remove the launchd background daemon")]
    Uninstall,
    #[command(about = "Restart the installed launchd background daemon")]
    Restart,
    #[command(about = "Show launchd daemon state as JSON")]
    Status,
    #[command(about = "Show recent daemon log lines")]
    Logs {
        #[arg(
            long,
            short = 'n',
            default_value_t = 50,
            value_parser = nonneg_int,
            allow_negative_numbers = true,
            help = "Number of lines to display from the end of daemon.log"
        )]
        lines: usize,
        #[arg(long, short = 'f', help = "Follow daemon.log as new lines are appended")]
        follow: bool,
    },
    #[command(about = "Run the sync loop and local HTTP API in the current process")]
    Run {
        #[arg(long, help = "Loopback HTTP port (default 47613 or TEAMS_CLI_PORT)")]
        port: Option<u16>,
        #[arg(
            long,
            default_value_t = paths::DEFAULT_SYNC_INTERVAL,
            value_parser = min_interval,
            help = "Seconds between lightweight cache refresh checks (minimum 10)"
        )]
        interval: u64,
        #[arg(long, help = "Serve the HTTP API without periodic syncing")]
        no_sync: bool,
        #[arg(long, help = "Enable scoped live write routes on the HTTP API")]
        allow_writes: bool,
        #[arg(
            long = "log-stderr",
            alias = "foreground",
            help = "Write daemon logs to stderr instead of daemon.log"
        )]
        log_stderr: bool,
    },
}

#[derive(Subcommand)]
pub enum ArchiveAction {
    #[command(about = "Write a consistent verified backup of the archive")]
    Backup {
        #[arg(
            long,
            value_hint = clap::ValueHint::FilePath,
            help = "Destination path (`.sqlite3` or `.sqlite3.xz`); omit for scheduled backup directory"
        )]
        out: Option<String>,
    },
    #[command(about = "Replace the archive with a verified backup (dry run unless --yes)")]
    Restore {
        #[arg(
            value_hint = clap::ValueHint::FilePath,
            help = "Path to the `.sqlite3` or `.sqlite3.xz` backup file"
        )]
        backup: String,
        #[arg(long, help = "Perform the restore (omit for dry-run verification)")]
        yes: bool,
        #[arg(
            long,
            help = "Do not save a safety backup of the current archive before replacing it"
        )]
        no_safety_backup: bool,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },
    #[command(about = "Prove the archive (or one backup FILE) reads end to end; exit 1 if not")]
    Verify {
        #[arg(
            value_hint = clap::ValueHint::FilePath,
            help = "Specific `.sqlite3` or `.sqlite3.xz` backup file to verify"
        )]
        file: Option<String>,
        #[arg(long, help = "Skip verifying files in the backups directory")]
        no_backups: bool,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },
    #[command(about = "Re-render every archived message with the current renderer")]
    Reindex,
    #[command(about = "Show archive schema version, applied migration ledger, and any pending upgrades")]
    Migrations {
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum DebugAction {
    #[command(about = "Raw Teams records kept in the archive")]
    Records {
        #[arg(help = "Raw store source name (omit to list available sources)")]
        source: Option<String>,
        #[arg(long, help = "Filter to a single record key")]
        key: Option<String>,
        #[arg(long, help = "Only versions captured at or after this time")]
        since: Option<String>,
        #[arg(long, help = "Return every captured version instead of only the latest per key")]
        all_versions: bool,
        #[arg(
            long,
            default_value_t = 50,
            value_parser = nonneg_int,
            allow_negative_numbers = true,
            help = "Maximum records to return (0 = all)"
        )]
        limit: usize,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },
    #[command(about = "List IndexedDB databases and object stores in the live Teams cache")]
    Stores,
    #[command(about = "Dump decoded JSON records from a specific database and store")]
    Dump {
        #[arg(help = "Substring matching a Teams IndexedDB database name")]
        db: String,
        #[arg(help = "Exact object store name")]
        store: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateNoticeMode {
    None,
    CheckOnly,
    Full,
}

impl Commands {
    pub fn is_json_output(&self) -> bool {
        match self {
            Commands::Inbox { json, .. }
            | Commands::Search { json, .. }
            | Commands::Mentions { json, .. }
            | Commands::Changes { json, .. }
            | Commands::People { json, .. }
            | Commands::Calls { json, .. }
            | Commands::Meetings { json, .. }
            | Commands::Sync { json, .. }
            | Commands::Status { json }
            | Commands::Doctor { json }
            | Commands::Update { json, .. } => *json,
            Commands::Presence { action, json } => match action {
                None => *json,
                Some(PresenceAction::Get { json: gj, .. }) => *gj || *json,
                Some(PresenceAction::Set { flags, .. }) => flags.json || *json,
                Some(PresenceAction::Message { action: ma }) => match ma {
                    PresenceMessageAction::Set { flags, .. } | PresenceMessageAction::Clear { flags } => {
                        flags.json || *json
                    }
                },
            },
            Commands::Chat { action } => match action {
                ChatAction::List { json, .. } => *json,
                ChatAction::Export { .. } => false,
                ChatAction::New { write, .. }
                | ChatAction::Rename { write, .. }
                | ChatAction::AddMember { write, .. }
                | ChatAction::RemoveMember { write, .. } => write.json,
                ChatAction::Read { flags, .. }
                | ChatAction::Unread { flags, .. }
                | ChatAction::Mute { flags, .. }
                | ChatAction::Unmute { flags, .. } => flags.json,
            },
            Commands::Message { action } => match action {
                MessageAction::List { json, .. }
                | MessageAction::Saved { json, .. }
                | MessageAction::Pinned { json, .. } => *json,
                MessageAction::Send { write, .. }
                | MessageAction::Edit { write, .. }
                | MessageAction::Delete { write, .. }
                | MessageAction::Forward { write, .. } => write.json,
                MessageAction::React { flags, .. }
                | MessageAction::Unreact { flags, .. }
                | MessageAction::Pin { flags, .. }
                | MessageAction::Unpin { flags, .. }
                | MessageAction::Save { flags, .. }
                | MessageAction::Unsave { flags, .. } => flags.json,
            },
            Commands::File { action } => match action {
                FileAction::List { json, .. } | FileAction::Fetch { json, .. } => *json,
            },
            Commands::Archive { action } => match action {
                ArchiveAction::Restore { json, .. }
                | ArchiveAction::Verify { json, .. }
                | ArchiveAction::Migrations { json } => *json,
                ArchiveAction::Backup { .. } | ArchiveAction::Reindex => false,
            },
            Commands::Token { action } => match action {
                TokenAction::Create { json, .. }
                | TokenAction::List { json, .. }
                | TokenAction::Revoke { json, .. }
                | TokenAction::Prune { json } => *json,
            },
            Commands::Debug { .. }
            | Commands::Daemon { .. }
            | Commands::Completions { .. }
            | Commands::Docs
            | Commands::Complete { .. } => false,
        }
    }

    pub fn update_notice_mode(&self) -> UpdateNoticeMode {
        match self {
            Commands::Docs
            | Commands::Complete { .. }
            | Commands::Completions { .. }
            | Commands::Debug { .. }
            | Commands::Token { .. }
            | Commands::Update { .. }
            | Commands::Chat {
                action: ChatAction::Export { .. },
            }
            | Commands::Daemon {
                action: DaemonAction::Run { .. } | DaemonAction::Status | DaemonAction::Logs { follow: true, .. },
            } => UpdateNoticeMode::None,
            Commands::Sync { json, notify, .. } => {
                if *json || *notify {
                    UpdateNoticeMode::None
                } else {
                    UpdateNoticeMode::Full
                }
            }
            Commands::Status { json } => {
                if *json {
                    UpdateNoticeMode::None
                } else {
                    UpdateNoticeMode::CheckOnly
                }
            }
            _ => {
                if self.is_json_output() {
                    UpdateNoticeMode::None
                } else {
                    UpdateNoticeMode::Full
                }
            }
        }
    }
}

pub enum CliError {
    Api(api::ApiError),
    Live(LiveError),
    Runtime(String),
}

impl From<api::ApiError> for CliError {
    fn from(e: api::ApiError) -> Self {
        CliError::Api(e)
    }
}

impl From<LiveError> for CliError {
    fn from(e: LiveError) -> Self {
        CliError::Live(e)
    }
}

impl From<archive::ArchiveError> for CliError {
    fn from(e: archive::ArchiveError) -> Self {
        CliError::Runtime(e.to_string())
    }
}

fn nonneg_int(v: &str) -> Result<usize, String> {
    let n: i64 = v.trim().parse().map_err(|_| format!("'{v}' is not a whole number"))?;
    if n < 0 {
        return Err(format!("must be 0 or more, not {n}"));
    }
    Ok(n as usize)
}

fn min_interval(v: &str) -> Result<u64, String> {
    let n: u64 = v
        .trim()
        .parse()
        .map_err(|_| format!("'{v}' is not a positive number of seconds"))?;
    if n < MIN_SYNC_INTERVAL {
        return Err(format!(
            "interval must be at least {MIN_SYNC_INTERVAL} seconds, not {n}"
        ));
    }
    Ok(n)
}

fn open_optional_archive() -> Result<Option<rusqlite::Connection>, CliError> {
    if paths::archive_path().exists() {
        Ok(Some(api::open_archive_readonly()?))
    } else {
        Ok(None)
    }
}

fn read_text_arg(words: Vec<String>, stdin: bool) -> Result<String, CliError> {
    if stdin {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| CliError::Runtime(e.to_string()))?;
        Ok(buf)
    } else {
        Ok(words.join(" "))
    }
}

fn load_media_paths(
    paths_list: &[PathBuf],
    max_count: usize,
    max_bytes: usize,
    kind: &str,
) -> Result<Vec<MediaUpload>, CliError> {
    if paths_list.len() > max_count {
        return Err(CliError::Live(LiveError::bad(format!(
            "Too many {kind}s ({}; max {max_count} per message).",
            paths_list.len()
        ))));
    }
    let mut out = Vec::with_capacity(paths_list.len());
    for raw_path in paths_list {
        let expanded = paths::expanduser(&raw_path.to_string_lossy());
        let meta = fs::metadata(&expanded)
            .map_err(|e| CliError::Live(LiveError::bad(format!("Cannot stat '{}': {e}", expanded.display()))))?;
        if meta.len() > max_bytes as u64 {
            return Err(CliError::Live(LiveError::bad(format!(
                "{kind} '{}' is {} bytes (max {max_bytes} bytes).",
                expanded.display(),
                meta.len()
            ))));
        }
        let file = fs::File::open(&expanded)
            .map_err(|e| CliError::Live(LiveError::bad(format!("Cannot read '{}': {e}", expanded.display()))))?;
        let mut bytes = Vec::with_capacity((meta.len() as usize).min(max_bytes));
        file.take((max_bytes + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|e| CliError::Live(LiveError::bad(format!("Cannot read '{}': {e}", expanded.display()))))?;
        if bytes.len() > max_bytes {
            return Err(CliError::Live(LiveError::bad(format!(
                "{kind} '{}' exceeds max size of {max_bytes} bytes.",
                expanded.display()
            ))));
        }
        let name = expanded
            .file_name()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| CliError::Live(LiveError::bad(format!("Invalid file path '{}'.", expanded.display()))))?
            .to_string();
        let ct = compose::guess_mime_from_filename(&name).to_string();
        out.push(MediaUpload {
            filename: name,
            content_type: ct,
            bytes,
        });
    }
    Ok(out)
}

pub fn run(cli: Cli) -> Result<i32, CliError> {
    match cli.command {
        Commands::Docs => {
            print!("{}", docs_text());
            Ok(0)
        }
        Commands::Complete { shell, words } => {
            let cmd = Cli::command();
            completions::print_complete(&cmd, &words, &shell);
            Ok(0)
        }
        Commands::Sync {
            source,
            force,
            notify,
            json,
        } => {
            let src_p = source.as_deref().map(PathBuf::from);
            let stats = sync::run_sync(src_p.as_deref(), notify, force).map_err(CliError::Runtime)?;
            if json {
                fmt::print_json(&stats);
                return Ok(0);
            }
            if let Some(sk) = stats.get("skipped").and_then(|v| v.as_str()) {
                println!("Nothing to do: {sk} (use --force to re-read anyway).");
            } else {
                let dur = stats.get("duration_ms").and_then(|v| v.as_f64()).unwrap_or(0.0) / 1000.0;
                println!(
                    "Synced in {dur:.1}s: {} new, {} edited, {} deleted, {} new chats (Teams cache holds {} messages).",
                    stats.get("messages_new").and_then(|v| v.as_i64()).unwrap_or(0),
                    stats.get("messages_edited").and_then(|v| v.as_i64()).unwrap_or(0),
                    stats.get("messages_deleted").and_then(|v| v.as_i64()).unwrap_or(0),
                    stats.get("conversations_new").and_then(|v| v.as_i64()).unwrap_or(0),
                    stats.get("cache_messages").and_then(|v| v.as_i64()).unwrap_or(0),
                );
            }
            if let Some(bk) = stats.get("backup").and_then(|v| v.as_str()) {
                println!("Backup written: {bk}");
            }
            if let Some(be) = stats.get("backup_error").and_then(|v| v.as_str()) {
                eprintln!("teams: backup failed: {be}");
                return Ok(1);
            }
            Ok(0)
        }
        Commands::Status { json } => {
            let conn = api::open_archive_readonly()?;
            let mut s = api::status(&conn)?;
            s["daemon"] = launchd::status();
            let healthy = s["healthy"].as_bool().unwrap_or(false);
            if json {
                fmt::print_json(&s);
                return Ok(if healthy { 0 } else { 1 });
            }
            let c = &s["counts"];
            println!(
                "Archive   {}  ({:.1} MB)",
                s["archive"]["path"].as_str().unwrap_or(""),
                s["archive"]["sizeBytes"].as_f64().unwrap_or(0.0) / 1e6
            );
            println!(
                "Contents  {} messages in {} chats, {} people, {} revisions, {} attachments",
                c["messages"], c["chats"], c["people"], c["revisions"], c["attachments"]
            );
            println!(
                "Range     {}  ->  {}",
                s["range"]["oldest"].as_str().unwrap_or("-"),
                s["range"]["newest"].as_str().unwrap_or("-")
            );
            println!(
                "Kept      {} messages that Teams no longer has in its cache",
                c["preservedAfterTeamsDropped"]
            );
            if !s["lastSync"].is_null() {
                let msg = s["lastSync"]["message"]
                    .as_str()
                    .map(|m| format!(" ({m})"))
                    .unwrap_or_default();
                println!(
                    "Last sync {} at {}{msg}",
                    s["lastSync"]["status"].as_str().unwrap_or(""),
                    s["lastSync"]["finishedAt"].as_str().unwrap_or("")
                );
            }
            if !s["lastSuccessfulSync"].is_null() && s["lastSuccessfulSync"]["startedAt"] != s["lastSync"]["startedAt"]
            {
                println!(
                    "Last OK   {}",
                    s["lastSuccessfulSync"]["finishedAt"].as_str().unwrap_or("")
                );
            }
            println!(
                "Checked   {} (cache unchanged since the last sync is skipped)",
                s["lastChecked"].as_str().unwrap_or("never")
            );
            let d = &s["daemon"];
            if d["loaded"].as_bool().unwrap_or(false) {
                println!(
                    "Daemon    {} (pid {})",
                    d["state"].as_str().unwrap_or("?"),
                    d["pid"].as_str().unwrap_or("?")
                );
            } else {
                println!("Daemon    not installed - run `teams daemon install`");
            }
            println!(
                "Backups   {} (latest: {})",
                s["backups"]["count"],
                s["backups"]["latest"].as_str().unwrap_or("none")
            );
            if let Some(latest) = update::peek_cached_update() {
                println!("Update    v{latest} available - run `teams update`");
            }
            if let Some(probs) = s["problems"].as_array() {
                for p in probs {
                    if let Some(ps) = p.as_str() {
                        println!("!         {ps}");
                    }
                }
            }
            Ok(if healthy { 0 } else { 1 })
        }
        Commands::Doctor { json } => {
            let res = doctor::run_diagnostics();
            let ok = res["ok"].as_bool().unwrap_or(false);
            if json {
                fmt::print_json(&res);
                return Ok(if ok { 0 } else { 1 });
            }
            println!(
                "teams-cli {} (schema v{}) — {}",
                res["version"].as_str().unwrap_or(VERSION),
                res["schemaVersion"],
                if ok { "OK" } else { "ISSUES FOUND" }
            );
            for c in res["checks"].as_array().into_iter().flatten() {
                let tag = if c["ok"].as_bool().unwrap_or(false) {
                    "ok"
                } else {
                    "FAIL"
                };
                println!(
                    "  [{tag:<4}] {:<14} {}",
                    c["name"].as_str().unwrap_or(""),
                    c["summary"].as_str().unwrap_or("")
                );
                if let Some(hint) = c["hint"].as_str().filter(|s| !s.is_empty()) {
                    println!("         -> {hint}");
                }
            }
            Ok(if ok { 0 } else { 1 })
        }
        Commands::Chat { action } => run_chat(action),
        Commands::Message { action } => run_message(action),
        Commands::File { action } => run_file(action),
        Commands::Presence { action, json } => run_presence(action, json),
        Commands::Search {
            query,
            chat,
            relevance,
            history,
            count,
            has_attachments,
            time,
            sender,
            limit,
            events,
            json,
        } => {
            let conn = api::open_archive_readonly()?;
            api::require_data(&conn)?;
            let q = query.join(" ");
            let r = api::search_with_options(
                &conn,
                Some(&q),
                chat.as_deref(),
                sender.as_deref(),
                time.since.as_deref(),
                time.until.as_deref(),
                limit,
                if relevance { "relevance" } else { "recent" },
                events,
                history,
                count,
                has_attachments,
            )?;
            if json {
                fmt::print_json(&r);
                return Ok(0);
            }
            if count {
                println!("{}", r["matched"]);
                fmt::note_senders(r.get("senders"), sender.as_deref());
                return Ok(0);
            }
            let results = r["results"].as_array().cloned().unwrap_or_default();
            for m in &results {
                let hist = m.get("matchedInHistory").and_then(|v| v.as_bool()).unwrap_or(false);
                let mut mc = m.clone();
                if hist {
                    if let Some(obj) = mc.as_object_mut() {
                        obj.insert("snippet".into(), Value::Null);
                    }
                }
                let mut text = fmt::fmt_message(&mc, "", m.get("chatName").and_then(|v| v.as_str()));
                if hist {
                    if let Some((head, rest)) = text.split_once('\n') {
                        text = format!("{head}  (matched earlier version)\n{rest}");
                    } else {
                        text.push_str("  (matched earlier version)");
                    }
                }
                println!("{text}");
            }
            let noun = if r["query"].is_string() { "matches" } else { "messages" };
            let hint = if r["truncated"].as_bool().unwrap_or(false) {
                " (use --limit 0 for all)"
            } else {
                ""
            };
            println!("\n# {} of {} {noun}{hint}", results.len(), r["matched"]);
            fmt::note_senders(r.get("senders"), sender.as_deref());
            Ok(0)
        }
        Commands::Changes {
            since,
            until,
            chat,
            edited,
            deleted,
            limit,
            json,
        } => {
            let conn = api::open_archive_readonly()?;
            api::require_data(&conn)?;
            let what = if edited {
                "edited"
            } else if deleted {
                "deleted"
            } else {
                "all"
            };
            let r = api::changes(&conn, Some(&since), until.as_deref(), chat.as_deref(), what, limit)?;
            if json {
                fmt::print_json(&r);
                return Ok(0);
            }
            let items = r["changes"].as_array().cloned().unwrap_or_default();
            for m in &items {
                println!(
                    "{}",
                    fmt::fmt_message(m, "", m.get("chatName").and_then(|v| v.as_str()))
                );
            }
            let hint = if r["truncated"].as_bool().unwrap_or(false) {
                " (use --limit 0 for all)"
            } else {
                ""
            };
            println!(
                "\n# {} of {} edited or deleted messages{hint}",
                items.len(),
                r["matched"]
            );
            Ok(0)
        }
        Commands::People {
            person,
            grep,
            bots,
            limit,
            json,
        } => {
            let conn = api::open_archive_readonly()?;
            api::require_data(&conn)?;
            if !person.is_empty() {
                let r = api::person(&conn, &person.join(" "), limit.unwrap_or(20))?;
                if json {
                    fmt::print_json(&r);
                    return Ok(0);
                }
                let p = &r["person"];
                if let Some(others) = r["otherMatches"].as_array().filter(|o| !o.is_empty()) {
                    let ns: Vec<String> = others
                        .iter()
                        .map(|o| {
                            let name = o["name"]
                                .as_str()
                                .or(o["email"].as_str())
                                .or(o["id"].as_str())
                                .unwrap_or("?");
                            format!("{name} ({})", o["messagesInArchive"])
                        })
                        .collect();
                    eprintln!(
                        "# Using {}. Other matches: {}",
                        Quoted(p["name"].as_str().unwrap_or("?")),
                        ns.join(", ")
                    );
                }
                let email = p["email"].as_str().map(|e| format!("  <{e}>")).unwrap_or_default();
                println!("{}{email}", p["name"].as_str().unwrap_or("?"));
                let role: Vec<&str> = [p["jobTitle"].as_str(), p["department"].as_str()]
                    .into_iter()
                    .flatten()
                    .filter(|s| !s.is_empty())
                    .collect();
                if !role.is_empty() {
                    println!("{}", role.join(" · "));
                }
                println!("id        {}", p["id"].as_str().unwrap_or(""));
                println!(
                    "messages  {}  (first {}, last {})",
                    p["messagesInArchive"],
                    p["firstContact"].as_str().unwrap_or("-"),
                    p["lastContact"].as_str().unwrap_or("-")
                );
                println!("\n# Shared chats ({})", r["totalSharedChats"]);
                if let Some(sc) = r["sharedChats"].as_array() {
                    for c in sc {
                        let cname: String = c["name"].as_str().unwrap_or("").chars().take(60).collect();
                        println!(
                            "{}  {:<9} {:>5} from them  {cname:<60}  {}",
                            short(c["lastActivityMs"].as_i64()),
                            c["kind"].as_str().unwrap_or(""),
                            c["messagesFromPerson"].as_i64().unwrap_or(0),
                            c["id"].as_str().unwrap_or("")
                        );
                    }
                }
                println!("\n# Recent files and links ({})", r["totalAttachments"]);
                if let Some(rf) = r["recentFiles"].as_array() {
                    for f in rf {
                        let k = f["kind"].as_str().unwrap_or("");
                        let u = f["url"].as_str().unwrap_or("");
                        let n = f["name"].as_str().unwrap_or("");
                        let name = if k == "image" || n == u || n.is_empty() {
                            String::new()
                        } else {
                            format!("{n}  ")
                        };
                        let where_s = f["chatName"].as_str().or(f["chatId"].as_str()).unwrap_or("");
                        println!("[{}] [{k}] {name}{u}  @ {where_s}", short(f["timeMs"].as_i64()));
                    }
                }
                return Ok(0);
            }
            let r = api::people(&conn, grep.as_deref(), limit.unwrap_or(50), bots)?;
            if json {
                fmt::print_json(&r);
                return Ok(0);
            }
            if let Some(ps) = r["people"].as_array() {
                for p in ps {
                    let n: String = p["name"].as_str().unwrap_or("?").chars().take(34).collect();
                    let e: String = p["email"].as_str().unwrap_or("").chars().take(40).collect();
                    let j: String = p["jobTitle"].as_str().unwrap_or("").chars().take(30).collect();
                    println!(
                        "{n:<34} {e:<40} {j:<30} {:>5}",
                        p["messagesInArchive"].as_i64().unwrap_or(0)
                    );
                }
            }
            Ok(0)
        }
        Commands::Inbox { since, limit, json } => {
            let conn = api::open_archive_readonly()?;
            api::require_data(&conn)?;
            let r = views::inbox(&conn, Some(&since), limit)?;
            if json {
                fmt::print_json(&r);
                return Ok(0);
            }
            let n = &r["counts"];
            println!(
                "# Inbox since {}: {} unread chats, {} mentions, {} awaiting your reply, {} unread activity",
                r["since"].as_str().unwrap_or("ever"),
                n["unreadChats"],
                n["mentions"],
                n["awaitingReply"],
                n["unreadActivities"]
            );
            println!("\n## Unread chats ({})", n["unreadChats"]);
            for u in r["unreadChats"].as_array().into_iter().flatten() {
                println!(
                    "{}  {:>3} new  {}  [{}]",
                    short(u["lastMessageMs"].as_i64()),
                    u["unreadCount"].as_i64().unwrap_or(0),
                    u["chatName"].as_str().or(u["chatId"].as_str()).unwrap_or(""),
                    u["chatId"].as_str().unwrap_or("")
                );
                println!(
                    "    {}: {}",
                    u["lastSender"].as_str().unwrap_or("?"),
                    fmt::line_preview(u["lastText"].as_str(), 100)
                );
            }
            println!("\n## Mentions ({})", n["mentions"]);
            for m in r["mentions"].as_array().into_iter().flatten() {
                let mark = if m["isRead"].as_bool() == Some(false) { "*" } else { " " };
                println!(
                    "{mark} {}  {}  @ {}  [{}]",
                    short(m["timeMs"].as_i64()),
                    m["sender"].as_str().filter(|s| !s.is_empty()).unwrap_or("?"),
                    m["chatName"].as_str().or(m["chatId"].as_str()).unwrap_or(""),
                    m["messageId"].as_str().unwrap_or("")
                );
                println!("    {}", fmt::line_preview(m["text"].as_str(), 100));
            }
            println!("\n## Awaiting your reply ({})", n["awaitingReply"]);
            for w in r["awaitingReply"].as_array().into_iter().flatten() {
                println!(
                    "{}  {}  [{}]",
                    short(w["lastMessageMs"].as_i64()),
                    w["chatName"].as_str().or(w["chatId"].as_str()).unwrap_or(""),
                    w["chatId"].as_str().unwrap_or("")
                );
                println!(
                    "    {}: {}",
                    w["lastSender"].as_str().unwrap_or("?"),
                    fmt::line_preview(w["lastText"].as_str(), 100)
                );
            }
            println!("\n## Unread activity ({})", n["unreadActivities"]);
            for x in r["unreadActivities"].as_array().into_iter().flatten() {
                let kind = match (x["activityType"].as_str(), x["activitySubtype"].as_str()) {
                    (Some(a), Some(b)) if !b.is_empty() => format!("{a}/{b}"),
                    (Some(a), _) => a.to_string(),
                    _ => "activity".to_string(),
                };
                println!(
                    "{}  {kind}  {}  @ {}",
                    short(x["timeMs"].as_i64()),
                    x["sender"].as_str().unwrap_or("?"),
                    x["chatName"].as_str().or(x["chatId"].as_str()).unwrap_or("-")
                );
                if let Some(pv) = x["preview"].as_str().filter(|s| !s.is_empty()) {
                    println!("    {}", fmt::line_preview(Some(pv), 100));
                }
            }
            Ok(0)
        }
        Commands::Mentions {
            time,
            chat,
            unread,
            limit,
            json,
        } => {
            let conn = api::open_archive_readonly()?;
            api::require_data(&conn)?;
            let r = views::mentions(
                &conn,
                time.since.as_deref(),
                time.until.as_deref(),
                chat.as_deref(),
                unread,
                limit,
            )?;
            if json {
                fmt::print_json(&r);
                return Ok(0);
            }
            for m in r["mentions"].as_array().into_iter().flatten() {
                let mark = match m["isRead"].as_bool() {
                    Some(false) => "unread",
                    Some(true) => "read",
                    None => "?",
                };
                println!(
                    "[{}] ({mark}) {}  @ {}  [{}]",
                    short(m["timeMs"].as_i64()),
                    m["sender"].as_str().filter(|s| !s.is_empty()).unwrap_or("?"),
                    m["chatName"].as_str().or(m["chatId"].as_str()).unwrap_or(""),
                    m["messageId"].as_str().unwrap_or("")
                );
                println!(
                    "{}",
                    fmt::indent(
                        m["text"].as_str().filter(|s| !s.is_empty()).unwrap_or("(no text)"),
                        "    "
                    )
                );
            }
            fmt::print_more(&r, "mentions", "mentions");
            Ok(0)
        }
        Commands::Calls {
            time,
            person,
            missed,
            limit,
            json,
        } => {
            let conn = api::open_archive_readonly()?;
            api::require_data(&conn)?;
            let r = views::calls(
                &conn,
                time.since.as_deref(),
                time.until.as_deref(),
                person.as_deref(),
                missed,
                limit,
            )?;
            if json {
                fmt::print_json(&r);
                return Ok(0);
            }
            for c in r["calls"].as_array().into_iter().flatten() {
                let who: Vec<&str> = c["participants"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|p| p["name"].as_str().or(p["id"].as_str()).unwrap_or("?"))
                    .collect();
                let wh = c["chatName"].as_str().map(|s| format!("  @ {s}")).unwrap_or_default();
                println!(
                    "[{}] {:<8} {:<9} {:>7}  {}{wh}",
                    short(c["timeMs"].as_i64()),
                    c["direction"].as_str().unwrap_or("?"),
                    c["state"].as_str().unwrap_or("?"),
                    fmt::duration(c["durationSeconds"].as_i64()),
                    who.join(", ")
                );
            }
            fmt::print_more(&r, "calls", "calls");
            Ok(0)
        }
        Commands::Meetings {
            time,
            grep,
            limit,
            json,
        } => {
            let conn = api::open_archive_readonly()?;
            api::require_data(&conn)?;
            let r = views::meetings(
                &conn,
                time.since.as_deref(),
                time.until.as_deref(),
                grep.as_deref(),
                limit,
            )?;
            if json {
                fmt::print_json(&r);
                return Ok(0);
            }
            for m in r["meetings"].as_array().into_iter().flatten() {
                let start_s = short(m["startMs"].as_i64());
                let mut end_s = m["endMs"]
                    .as_i64()
                    .map(|t| short(Some(t)))
                    .unwrap_or_else(|| "?".to_string());
                if start_s.len() >= 10 && end_s.len() >= 16 && start_s[..10] == end_s[..10] {
                    end_s = end_s[11..].to_string();
                }
                println!(
                    "[{start_s} – {end_s}] {}",
                    m["subject"].as_str().unwrap_or("(no subject)")
                );
                let loc = m["location"].as_str().map(|l| format!(" · {l}")).unwrap_or_default();
                let att_n = m["attendees"].as_array().map(|a| a.len()).unwrap_or(0);
                println!(
                    "    organizer {} · you: {} · {att_n} attendees{loc}",
                    m["organizer"].as_str().unwrap_or("?"),
                    m["myResponse"].as_str().unwrap_or("?")
                );
                let mut link = m["joinUrl"].as_str().unwrap_or("").to_string();
                if let Some(cid) = m["chatId"].as_str() {
                    if !link.is_empty() {
                        link.push_str("  ");
                    }
                    link.push_str(&format!("chat {cid}"));
                }
                if !link.is_empty() {
                    println!("    {link}");
                }
            }
            fmt::print_more(&r, "meetings", "meetings");
            Ok(0)
        }
        Commands::Archive { action } => run_archive(action),
        Commands::Daemon { action } => run_daemon(action),
        Commands::Token { action } => run_token(action),
        Commands::Debug { action } => run_debug(action),
        Commands::Completions { shell } => {
            print!("{}", completions::generate_script(shell));
            Ok(0)
        }
        Commands::Update {
            check,
            target_version,
            force,
            json,
            background_check,
        } => {
            if background_check {
                update::run_background_check();
                return Ok(0);
            }
            let opts = update::UpdateOptions {
                check_only: check,
                version: target_version,
                force,
            };
            let outcome = update::run_update(&opts).map_err(CliError::Runtime)?;
            if json {
                fmt::print_json(&outcome.to_json());
            } else {
                let color =
                    std::io::IsTerminal::is_terminal(&std::io::stdout()) && std::env::var_os("NO_COLOR").is_none();
                let (bold, dim, green, reset) = if color {
                    ("\x1b[1m", "\x1b[2m", "\x1b[32m", "\x1b[0m")
                } else {
                    ("", "", "", "")
                };
                if outcome.updated {
                    println!(
                        "{bold}teams-cli v{} -> v{}{reset} {dim}({}){reset}",
                        outcome.current_version, outcome.target_version, outcome.target_triple
                    );
                    if let Some(ref hash) = outcome.sha256 {
                        let short_h = &hash[..hash.len().min(12)];
                        println!("  {green}{:<12}{reset} sha256 {short_h}...", "verified");
                    }
                    println!("  {green}{:<12}{reset} {}", "installed", outcome.binary_path);
                    if outcome.daemon_restarted {
                        println!("  {green}{:<12}{reset} restarted", "daemon");
                    }
                } else if outcome.update_available {
                    println!(
                        "{bold}teams-cli v{} -> v{}{reset} {dim}(update available){reset}",
                        outcome.current_version, outcome.target_version
                    );
                    println!("  {dim}{:<12}{reset} teams update", "run");
                } else {
                    println!(
                        "{bold}teams-cli v{}{reset} {green}(up to date){reset}",
                        outcome.current_version
                    );
                }
            }
            if let Some(ref err) = outcome.daemon_restart_error {
                eprintln!("teams: daemon restart failed ({err}); run `teams daemon restart`");
                return Ok(1);
            }
            Ok(0)
        }
    }
}

fn run_chat(action: ChatAction) -> Result<i32, CliError> {
    let live = LiveService::default();
    let principal = Principal::cli();
    match action {
        ChatAction::List {
            grep,
            kind,
            all,
            member,
            limit,
            json,
        } => {
            let conn = api::open_archive_readonly()?;
            api::require_data(&conn)?;
            let r = api::list_chats(&conn, grep.as_deref(), kind.as_deref(), limit, all, member.as_deref())?;
            if json {
                fmt::print_json(&r);
                return Ok(0);
            }
            if let Some(mm) = r.get("memberMatches").and_then(|v| v.as_array()) {
                if mm.len() > 1 {
                    let names: Vec<&str> = mm
                        .iter()
                        .take(8)
                        .filter_map(|m| m["name"].as_str().or(m["email"].as_str()).or(m["id"].as_str()))
                        .collect();
                    eprintln!(
                        "# --member {} matched {} people: {}. Use a fuller name or their email to pick one.",
                        Quoted(member.as_deref().unwrap_or("")),
                        mm.len(),
                        names.join(", ")
                    );
                }
            }
            let chats = r["chats"].as_array().cloned().unwrap_or_default();
            for c in &chats {
                let name: String = c["name"].as_str().unwrap_or("").chars().take(70).collect();
                println!(
                    "{}  {:<9} {:>5}  {:<70}  {}",
                    short(c["lastActivityMs"].as_i64()),
                    c["kind"].as_str().unwrap_or(""),
                    c["messageCount"].as_i64().unwrap_or(0),
                    name,
                    c["id"].as_str().unwrap_or("")
                );
            }
            let total = r["total"].as_u64().unwrap_or(0) as usize;
            if total > chats.len() {
                println!("... {} more (use --limit 0 for all)", total - chats.len());
            }
            Ok(0)
        }
        ChatAction::Export {
            chat,
            time,
            sender,
            thread,
            events,
            history,
            out,
        } => {
            let conn = api::open_archive_readonly()?;
            api::require_data(&conn)?;
            let md = export::export_markdown(
                &conn,
                &chat,
                time.since.as_deref(),
                time.until.as_deref(),
                sender.as_deref(),
                events,
                history,
                thread.as_deref(),
            )?;
            if let Some(out_s) = out {
                let dest = paths::expanduser(&out_s);
                paths::atomic_write_text(&dest, &md).map_err(|e| CliError::Runtime(e.to_string()))?;
                eprintln!("Wrote {}", dest.display());
            } else {
                print!("{md}");
            }
            Ok(0)
        }
        ChatAction::New { members, topic, write } => {
            let conn = open_optional_archive()?;
            let archive = conn.as_ref().map_or(ArchiveSource::None, ArchiveSource::Conn);
            let res = live.create_chat(
                archive,
                &principal,
                CreateChatRequest {
                    members,
                    topic,
                    dry_run: write.is_dry_run(),
                },
            )?;
            if write.json {
                fmt::print_json(&res);
                return Ok(0);
            }
            if res["dryRun"].as_bool().unwrap_or(false) {
                println!(
                    "[dry run] Would create chat with {} member(s).",
                    res["members"].as_array().map_or(0, Vec::len)
                );
                println!("Re-run with {}=1 and --yes to execute.", policy::ENABLE_WRITES_ENV);
                return Ok(0);
            }
            println!(
                "Created chat {}.",
                res["conversationId"].as_str().unwrap_or("(pending)")
            );
            Ok(0)
        }
        ChatAction::Rename { chat, topic, write } => {
            let conn = open_optional_archive()?;
            let archive = conn.as_ref().map_or(ArchiveSource::None, ArchiveSource::Conn);
            let res = live.rename_chat(
                archive,
                &principal,
                RenameChatRequest {
                    chat,
                    topic,
                    dry_run: write.is_dry_run(),
                },
            )?;
            if write.json {
                fmt::print_json(&res);
                return Ok(0);
            }
            if res["dryRun"].as_bool().unwrap_or(false) {
                println!(
                    "[dry run] Would rename {} to {}.",
                    res["chatName"].as_str().unwrap_or(""),
                    Quoted(res["topic"].as_str().unwrap_or(""))
                );
                println!("Re-run with {}=1 and --yes to execute.", policy::ENABLE_WRITES_ENV);
                return Ok(0);
            }
            println!(
                "Renamed {} ({}) to {}.",
                res["previousName"].as_str().unwrap_or(""),
                res["conversationId"].as_str().unwrap_or(""),
                Quoted(res["topic"].as_str().unwrap_or(""))
            );
            Ok(0)
        }
        ChatAction::AddMember { chat, member, write } => {
            let conn = open_optional_archive()?;
            let archive = conn.as_ref().map_or(ArchiveSource::None, ArchiveSource::Conn);
            let res = live.change_member(
                archive,
                &principal,
                MemberChangeRequest {
                    chat,
                    member,
                    add: true,
                    dry_run: write.is_dry_run(),
                },
            )?;
            if write.json {
                fmt::print_json(&res);
                return Ok(0);
            }
            if res["dryRun"].as_bool().unwrap_or(false) {
                println!(
                    "[dry run] Would add {} to {}.",
                    res["member"]["name"].as_str().unwrap_or(""),
                    res["chatName"].as_str().unwrap_or("")
                );
                println!("Re-run with {}=1 and --yes to execute.", policy::ENABLE_WRITES_ENV);
                return Ok(0);
            }
            println!(
                "Added {} to {} ({}).",
                res["member"]["name"].as_str().unwrap_or(""),
                res["chatName"].as_str().unwrap_or(""),
                res["conversationId"].as_str().unwrap_or("")
            );
            Ok(0)
        }
        ChatAction::RemoveMember { chat, member, write } => {
            let conn = open_optional_archive()?;
            let archive = conn.as_ref().map_or(ArchiveSource::None, ArchiveSource::Conn);
            let res = live.change_member(
                archive,
                &principal,
                MemberChangeRequest {
                    chat,
                    member,
                    add: false,
                    dry_run: write.is_dry_run(),
                },
            )?;
            if write.json {
                fmt::print_json(&res);
                return Ok(0);
            }
            if res["dryRun"].as_bool().unwrap_or(false) {
                println!(
                    "[dry run] Would remove {} from {}.",
                    res["member"]["name"].as_str().unwrap_or(""),
                    res["chatName"].as_str().unwrap_or("")
                );
                println!("Re-run with {}=1 and --yes to execute.", policy::ENABLE_WRITES_ENV);
                return Ok(0);
            }
            println!(
                "Removed {} from {} ({}).",
                res["member"]["name"].as_str().unwrap_or(""),
                res["chatName"].as_str().unwrap_or(""),
                res["conversationId"].as_str().unwrap_or("")
            );
            Ok(0)
        }
        ChatAction::Read { chat, flags } => run_chat_read_state(&live, &principal, chat, flags, true),
        ChatAction::Unread { chat, flags } => run_chat_read_state(&live, &principal, chat, flags, false),
        ChatAction::Mute { chat, flags } => run_chat_mute_state(&live, &principal, chat, flags, true),
        ChatAction::Unmute { chat, flags } => run_chat_mute_state(&live, &principal, chat, flags, false),
    }
}

fn run_chat_read_state(
    live: &LiveService,
    principal: &Principal,
    chat: String,
    flags: LightWriteArgs,
    mark_read: bool,
) -> Result<i32, CliError> {
    let conn = open_optional_archive()?;
    let archive = conn.as_ref().map_or(ArchiveSource::None, ArchiveSource::Conn);
    let res = live.set_read_state(
        archive,
        principal,
        ChatStateRequest {
            chat,
            dry_run: flags.dry_run,
        },
        mark_read,
    )?;
    if flags.json {
        fmt::print_json(&res);
        return Ok(0);
    }
    let verb = if mark_read { "read" } else { "unread" };
    if res["dryRun"].as_bool().unwrap_or(false) {
        println!(
            "[dry run] Would mark {} as {verb}.",
            res["chatName"].as_str().unwrap_or("")
        );
    } else {
        println!(
            "Marked {} ({}) as {verb}.",
            res["chatName"].as_str().unwrap_or(""),
            res["conversationId"].as_str().unwrap_or("")
        );
    }
    Ok(0)
}

fn run_chat_mute_state(
    live: &LiveService,
    principal: &Principal,
    chat: String,
    flags: LightWriteArgs,
    muted: bool,
) -> Result<i32, CliError> {
    let conn = open_optional_archive()?;
    let archive = conn.as_ref().map_or(ArchiveSource::None, ArchiveSource::Conn);
    let res = live.set_mute_state(
        archive,
        principal,
        ChatStateRequest {
            chat,
            dry_run: flags.dry_run,
        },
        muted,
    )?;
    if flags.json {
        fmt::print_json(&res);
        return Ok(0);
    }
    let verb = if muted { "Muted" } else { "Unmuted" };
    if res["dryRun"].as_bool().unwrap_or(false) {
        println!(
            "[dry run] Would set muted={muted} on {}.",
            res["chatName"].as_str().unwrap_or("")
        );
    } else {
        println!(
            "{verb} {} ({}).",
            res["chatName"].as_str().unwrap_or(""),
            res["conversationId"].as_str().unwrap_or("")
        );
    }
    Ok(0)
}

fn run_message(action: MessageAction) -> Result<i32, CliError> {
    let live = LiveService::default();
    let principal = Principal::cli();
    match action {
        MessageAction::List {
            chat,
            around,
            context,
            thread,
            history,
            time,
            sender,
            limit,
            events,
            json,
        } => {
            let conn = api::open_archive_readonly()?;
            api::require_data(&conn)?;
            let r = api::read_messages(
                &conn,
                chat.as_deref(),
                time.since.as_deref(),
                time.until.as_deref(),
                sender.as_deref(),
                limit,
                events,
                thread.as_deref(),
                history,
                around.as_deref(),
                context,
            )?;
            if json {
                fmt::print_json(&r);
                return Ok(0);
            }
            let c = &r["chat"];
            if let Some(others) = r["otherMatches"].as_array().filter(|o| !o.is_empty()) {
                eprintln!("# Using {}. Other matches:", Quoted(c["name"].as_str().unwrap_or("")));
                for o in others {
                    eprintln!(
                        "#   {}  [{}]",
                        o["name"].as_str().unwrap_or(""),
                        o["id"].as_str().unwrap_or("")
                    );
                }
            }
            let msgs = r["messages"].as_array().cloned().unwrap_or_default();
            if let Some(target) = r.get("aroundMessageId").and_then(|v| v.as_str()) {
                println!(
                    "# {}  ({}, {} messages around {target})\n",
                    c["name"].as_str().unwrap_or(""),
                    c["kind"].as_str().unwrap_or(""),
                    msgs.len()
                );
                if r["hasBefore"].as_bool().unwrap_or(false) {
                    println!("# earlier messages exist\n");
                }
                for m in &msgs {
                    let pfx = if m["id"].as_str() == Some(target) { ">>> " } else { "" };
                    println!("{}", fmt::fmt_message(m, pfx, None));
                }
                if r["hasAfter"].as_bool().unwrap_or(false) {
                    println!("\n# later messages exist");
                }
                return Ok(0);
            }
            println!(
                "# {}  ({}, showing {} of {})\n",
                c["name"].as_str().unwrap_or(""),
                c["kind"].as_str().unwrap_or(""),
                msgs.len(),
                r["matched"]
            );
            if c["kind"].as_str() == Some("channel") {
                fmt::print_threaded(&msgs);
            } else {
                for m in &msgs {
                    println!("{}", fmt::fmt_message(m, "", None));
                }
            }
            if r["truncated"].as_bool().unwrap_or(false) {
                let matched = r["matched"].as_u64().unwrap_or(0) as usize;
                println!(
                    "\n# {} earlier messages not shown (use --limit 0 or --since)",
                    matched.saturating_sub(msgs.len())
                );
            }
            Ok(0)
        }
        MessageAction::Saved {
            include_unsaved,
            limit,
            json,
        } => {
            let conn = api::open_archive_readonly()?;
            api::require_data(&conn)?;
            let r = views::saved(&conn, include_unsaved, limit)?;
            if json {
                fmt::print_json(&r);
                return Ok(0);
            }
            let items = r["saved"].as_array().cloned().unwrap_or_default();
            fmt::print_items(&items, "savedAtMs", |x| {
                let mut s = String::new();
                if x["unsaved"].as_bool().unwrap_or(false) {
                    s.push_str("  (unsaved)");
                }
                if x["deleted"].as_bool().unwrap_or(false) {
                    s.push_str("  (message deleted)");
                }
                s
            });
            fmt::print_more(&r, "saved", "saved messages");
            Ok(0)
        }
        MessageAction::Pinned { chat, limit, json } => {
            let conn = api::open_archive_readonly()?;
            api::require_data(&conn)?;
            let r = views::pinned(&conn, chat.as_deref(), limit)?;
            if json {
                fmt::print_json(&r);
                return Ok(0);
            }
            let items = r["pinned"].as_array().cloned().unwrap_or_default();
            fmt::print_items(&items, "timeMs", |_| String::new());
            fmt::print_more(&r, "pinned", "pinned messages");
            Ok(0)
        }
        MessageAction::Send {
            target,
            message,
            stdin,
            reply_to,
            quote,
            subject,
            important,
            urgent,
            markdown,
            html,
            mention,
            image,
            file,
            client_message_id,
            idempotency_key,
            write,
        } => {
            let text = read_text_arg(message, stdin)?;
            let images = load_media_paths(
                &image,
                crate::live::service::messages::MAX_IMAGES_PER_MESSAGE,
                ams::MAX_IMAGE_BYTES,
                "Image",
            )?;
            let files = load_media_paths(
                &file,
                crate::live::service::messages::MAX_FILES_PER_MESSAGE,
                sharepoint::MAX_SIMPLE_FILE_BYTES,
                "File",
            )?;
            let format = if html {
                BodyFormat::Html
            } else if markdown {
                BodyFormat::Markdown
            } else {
                BodyFormat::Plain
            };
            let importance = if urgent {
                Importance::Urgent
            } else if important {
                Importance::High
            } else {
                Importance::Normal
            };
            let conn = open_optional_archive()?;
            let archive = conn.as_ref().map_or(ArchiveSource::None, ArchiveSource::Conn);
            let res = live.send_message(
                archive,
                &principal,
                SendMessageRequest {
                    target,
                    text,
                    format,
                    reply_to,
                    quote,
                    subject,
                    importance,
                    mentions: mention,
                    images,
                    files,
                    client_message_id,
                    idempotency_key,
                    dry_run: write.is_dry_run(),
                },
            )?;
            if write.json {
                fmt::print_json(&res);
                return Ok(0);
            }
            if res["dryRun"].as_bool().unwrap_or(false) {
                let status = if res["restricted"].as_bool().unwrap_or(true) {
                    format!("RESTRICTED (set {}=1 to enable)", policy::ENABLE_WRITES_ENV)
                } else {
                    "enabled".to_string()
                };
                let allow = if res["allowed"].as_bool().unwrap_or(true) {
                    ""
                } else {
                    "  [BLOCKED by TEAMS_CLI_WRITE_ALLOWLIST]"
                };
                println!("[dry run; live writes are {status}]{allow}");
                println!(
                    "  To:      {}  ({}, {})",
                    res["chatName"].as_str().unwrap_or(""),
                    res["chatKind"].as_str().unwrap_or(""),
                    res["conversationId"].as_str().unwrap_or("")
                );
                if let Some(rt) = res["replyTo"].as_str() {
                    println!("  Reply:   thread {rt}");
                }
                if let Some(q) = res["quoteMessageId"].as_str() {
                    println!("  Quote:   message {q}");
                }
                if let Some(s) = res["subject"].as_str() {
                    println!("  Subject: {s}");
                }
                if let Some(imp) = res["importance"].as_str() {
                    println!("  Level:   {imp}");
                }
                if let Some(ms) = res["mentions"].as_array().filter(|a| !a.is_empty()) {
                    let names: Vec<&str> = ms.iter().filter_map(|m| m["name"].as_str()).collect();
                    println!("  Mention: {}", names.join(", "));
                }
                if let Some(imgs) = res["images"].as_array().filter(|a| !a.is_empty()) {
                    let names: Vec<&str> = imgs.iter().filter_map(|i| i["name"].as_str()).collect();
                    println!("  Images:  {}", names.join(", "));
                }
                if let Some(fs) = res["files"].as_array().filter(|a| !a.is_empty()) {
                    let names: Vec<&str> = fs.iter().filter_map(|f| f["name"].as_str()).collect();
                    println!("  Files:   {}", names.join(", "));
                }
                println!("  Text:    {}", res["text"].as_str().unwrap_or(""));
                println!(
                    "Nothing was sent. Re-run with {}=1 and --yes to send.",
                    policy::ENABLE_WRITES_ENV
                );
                return Ok(0);
            }
            println!(
                "Sent message {} to {} ({}).",
                res["messageId"].as_str().unwrap_or("?"),
                res["chatName"].as_str().unwrap_or(""),
                res["conversationId"].as_str().unwrap_or("")
            );
            Ok(0)
        }
        MessageAction::Edit {
            message,
            text,
            chat,
            stdin,
            markdown,
            html,
            write,
        } => {
            let body = read_text_arg(text, stdin)?;
            let format = if html {
                BodyFormat::Html
            } else if markdown {
                BodyFormat::Markdown
            } else {
                BodyFormat::Plain
            };
            let conn = open_optional_archive()?;
            let archive = conn.as_ref().map_or(ArchiveSource::None, ArchiveSource::Conn);
            let res = live.edit_message(
                archive,
                &principal,
                EditMessageRequest {
                    chat,
                    message_id: message,
                    text: body,
                    format,
                    dry_run: write.is_dry_run(),
                },
            )?;
            if write.json {
                fmt::print_json(&res);
                return Ok(0);
            }
            if res["dryRun"].as_bool().unwrap_or(false) {
                println!(
                    "[dry run] Would edit message {} in {}.",
                    res["messageId"].as_str().unwrap_or(""),
                    res["chatName"].as_str().unwrap_or("")
                );
                println!("Re-run with {}=1 and --yes to execute.", policy::ENABLE_WRITES_ENV);
                return Ok(0);
            }
            println!(
                "Edited message {} in {} ({}).",
                res["messageId"].as_str().unwrap_or(""),
                res["chatName"].as_str().unwrap_or(""),
                res["conversationId"].as_str().unwrap_or("")
            );
            Ok(0)
        }
        MessageAction::Delete { message, chat, write } => {
            let conn = open_optional_archive()?;
            let archive = conn.as_ref().map_or(ArchiveSource::None, ArchiveSource::Conn);
            let res = live.delete_message(
                archive,
                &principal,
                MessageActionRequest {
                    chat,
                    message_id: message,
                    dry_run: write.is_dry_run(),
                },
            )?;
            if write.json {
                fmt::print_json(&res);
                return Ok(0);
            }
            if res["dryRun"].as_bool().unwrap_or(false) {
                println!(
                    "[dry run] Would delete message {} in {}.",
                    res["messageId"].as_str().unwrap_or(""),
                    res["chatName"].as_str().unwrap_or("")
                );
                println!("Re-run with {}=1 and --yes to execute.", policy::ENABLE_WRITES_ENV);
                return Ok(0);
            }
            println!(
                "Deleted message {} in {} ({}).",
                res["messageId"].as_str().unwrap_or(""),
                res["chatName"].as_str().unwrap_or(""),
                res["conversationId"].as_str().unwrap_or("")
            );
            Ok(0)
        }
        MessageAction::React {
            message,
            reaction,
            chat,
            flags,
        } => run_message_react(&live, &principal, message, reaction, chat, flags, true),
        MessageAction::Unreact {
            message,
            reaction,
            chat,
            flags,
        } => run_message_react(&live, &principal, message, reaction, chat, flags, false),
        MessageAction::Pin { message, chat, flags } => run_message_pin(&live, &principal, message, chat, flags, true),
        MessageAction::Unpin { message, chat, flags } => {
            run_message_pin(&live, &principal, message, chat, flags, false)
        }
        MessageAction::Save { message, chat, flags } => run_message_save(&live, &principal, message, chat, flags, true),
        MessageAction::Unsave { message, chat, flags } => {
            run_message_save(&live, &principal, message, chat, flags, false)
        }
        MessageAction::Forward {
            message,
            to_chat,
            from_chat,
            comment,
            markdown,
            idempotency_key,
            write,
        } => {
            let conn = open_optional_archive()?;
            let archive = conn.as_ref().map_or(ArchiveSource::None, ArchiveSource::Conn);
            let res = live.forward_message(
                archive,
                &principal,
                ForwardMessageRequest {
                    from_chat,
                    message_id: message,
                    to_chat,
                    comment: comment.unwrap_or_default(),
                    format: if markdown {
                        BodyFormat::Markdown
                    } else {
                        BodyFormat::Plain
                    },
                    idempotency_key,
                    dry_run: write.is_dry_run(),
                },
            )?;
            if write.json {
                fmt::print_json(&res);
                return Ok(0);
            }
            if res["dryRun"].as_bool().unwrap_or(false) {
                println!(
                    "[dry run] Would forward message {} to {}.",
                    res["fromMessageId"].as_str().unwrap_or(""),
                    res["toChatName"].as_str().unwrap_or("")
                );
                println!("Re-run with {}=1 and --yes to execute.", policy::ENABLE_WRITES_ENV);
                return Ok(0);
            }
            println!(
                "Forwarded message {} to {} ({}) as {}.",
                res["fromMessageId"].as_str().unwrap_or(""),
                res["toChatName"].as_str().unwrap_or(""),
                res["toConversationId"].as_str().unwrap_or(""),
                res["messageId"].as_str().unwrap_or("?")
            );
            Ok(0)
        }
    }
}

fn run_message_react(
    live: &LiveService,
    principal: &Principal,
    message: String,
    reaction: String,
    chat: Option<String>,
    flags: LightWriteArgs,
    add: bool,
) -> Result<i32, CliError> {
    let conn = open_optional_archive()?;
    let archive = conn.as_ref().map_or(ArchiveSource::None, ArchiveSource::Conn);
    let res = live.react_message(
        archive,
        principal,
        ReactMessageRequest {
            chat,
            message_id: message,
            reaction,
            add,
            dry_run: flags.dry_run,
        },
    )?;
    if flags.json {
        fmt::print_json(&res);
        return Ok(0);
    }
    let verb = if add { "Reacted" } else { "Removed reaction" };
    if res["dryRun"].as_bool().unwrap_or(false) {
        println!(
            "[dry run] Would {} '{}' on message {}.",
            if add { "add" } else { "remove" },
            res["reaction"].as_str().unwrap_or(""),
            res["messageId"].as_str().unwrap_or("")
        );
    } else {
        println!(
            "{verb} '{}' on message {} in {}.",
            res["reaction"].as_str().unwrap_or(""),
            res["messageId"].as_str().unwrap_or(""),
            res["chatName"].as_str().unwrap_or("")
        );
    }
    Ok(0)
}

fn run_message_pin(
    live: &LiveService,
    principal: &Principal,
    message: String,
    chat: Option<String>,
    flags: LightWriteArgs,
    pin: bool,
) -> Result<i32, CliError> {
    let conn = open_optional_archive()?;
    let archive = conn.as_ref().map_or(ArchiveSource::None, ArchiveSource::Conn);
    let res = live.pin_message(
        archive,
        principal,
        MessageActionRequest {
            chat,
            message_id: message,
            dry_run: flags.dry_run,
        },
        pin,
    )?;
    if flags.json {
        fmt::print_json(&res);
        return Ok(0);
    }
    let verb = if pin { "Pinned" } else { "Unpinned" };
    if res["dryRun"].as_bool().unwrap_or(false) {
        println!(
            "[dry run] Would {} message {}.",
            if pin { "pin" } else { "unpin" },
            res["messageId"].as_str().unwrap_or("")
        );
    } else {
        println!(
            "{verb} message {} in {}.",
            res["messageId"].as_str().unwrap_or(""),
            res["chatName"].as_str().unwrap_or("")
        );
    }
    Ok(0)
}

fn run_message_save(
    live: &LiveService,
    principal: &Principal,
    message: String,
    chat: Option<String>,
    flags: LightWriteArgs,
    save: bool,
) -> Result<i32, CliError> {
    let conn = open_optional_archive()?;
    let archive = conn.as_ref().map_or(ArchiveSource::None, ArchiveSource::Conn);
    let res = live.save_message(
        archive,
        principal,
        MessageActionRequest {
            chat,
            message_id: message,
            dry_run: flags.dry_run,
        },
        save,
    )?;
    if flags.json {
        fmt::print_json(&res);
        return Ok(0);
    }
    let verb = if save { "Saved" } else { "Unsaved" };
    if res["dryRun"].as_bool().unwrap_or(false) {
        println!(
            "[dry run] Would {} message {}.",
            if save { "save" } else { "unsave" },
            res["messageId"].as_str().unwrap_or("")
        );
    } else {
        println!(
            "{verb} message {} in {}.",
            res["messageId"].as_str().unwrap_or(""),
            res["chatName"].as_str().unwrap_or("")
        );
    }
    Ok(0)
}

fn run_file(action: FileAction) -> Result<i32, CliError> {
    match action {
        FileAction::List {
            query,
            chat,
            kind,
            file_type,
            time,
            sender,
            limit,
            json,
        } => {
            let conn = api::open_archive_readonly()?;
            api::require_data(&conn)?;
            let q = query.join(" ");
            let q_opt = if q.is_empty() { None } else { Some(q.as_str()) };
            let r = api::attachments(
                &conn,
                q_opt,
                chat.as_deref(),
                sender.as_deref(),
                kind.as_deref(),
                file_type.as_deref(),
                time.since.as_deref(),
                time.until.as_deref(),
                limit,
            )?;
            if json {
                fmt::print_json(&r);
                return Ok(0);
            }
            let atts = r["attachments"].as_array().cloned().unwrap_or_default();
            for x in &atts {
                let where_s = x["chatName"].as_str().or(x["chatId"].as_str()).unwrap_or("?");
                let k = x["kind"].as_str().unwrap_or("");
                let u = x["url"].as_str().unwrap_or("");
                let n = x["name"].as_str().unwrap_or("");
                let name = if k == "image" || n == u || n.is_empty() {
                    String::new()
                } else {
                    format!("{n}  ")
                };
                let gone = if x["messageDeleted"].as_bool().unwrap_or(false) {
                    "  (message deleted)"
                } else {
                    ""
                };
                println!(
                    "[{}] {}  @ {where_s}{gone}\n    [{k}] {name}{u}",
                    short(x["timeMs"].as_i64()),
                    x["sender"].as_str().filter(|s| !s.is_empty()).unwrap_or("?")
                );
                if let Some(lp) = x["localPath"].as_str() {
                    println!("    [local] {lp}");
                }
            }
            let hint = if r["truncated"].as_bool().unwrap_or(false) {
                " (use --limit 0 for all)"
            } else {
                ""
            };
            println!("\n# {} of {} files{hint}", atts.len(), r["matched"]);
            fmt::note_senders(r.get("senders"), sender.as_deref());
            Ok(0)
        }
        FileAction::Fetch {
            target,
            message,
            index,
            out,
            force_download,
            max_bytes,
            json,
        } => {
            let joined = target.join(" ");
            let t_opt = if joined.trim().is_empty() {
                None
            } else {
                Some(joined.trim())
            };
            if t_opt.is_none() && message.is_none() {
                return Err(CliError::Api(api::ApiError::bad(
                    "bad_param",
                    "Provide a URL, message id, filename query, or --message MSG_ID.",
                )));
            }
            let is_url = t_opt
                .map(|t| {
                    let l = t.to_ascii_lowercase();
                    l.starts_with("https://") || l.starts_with("http://")
                })
                .unwrap_or(false);
            let conn = if is_url {
                open_optional_archive()?
            } else {
                let c = api::open_archive_readonly()?;
                api::require_data(&c)?;
                Some(c)
            };
            let res = fetch::fetch_attachment_with_limit(
                conn.as_ref(),
                t_opt,
                message.as_deref(),
                index,
                out.as_deref(),
                force_download,
                max_bytes,
            )
            .map_err(|e| CliError::Runtime(e.to_string()))?;
            if json {
                fmt::print_json(&res);
                return Ok(0);
            }
            println!(
                "{}  ({} bytes, {}, {})",
                res["path"].as_str().unwrap_or(""),
                res["sizeBytes"],
                res["contentType"].as_str().unwrap_or(""),
                res["source"].as_str().unwrap_or("")
            );
            Ok(0)
        }
    }
}

fn run_presence(action: Option<PresenceAction>, top_json: bool) -> Result<i32, CliError> {
    let live = LiveService::default();
    let principal = Principal::cli();
    match action {
        None => {
            let res = live.get_presence(ArchiveSource::None, &principal, &[])?;
            if top_json {
                fmt::print_json(&res);
            } else {
                println!(
                    "{} ({})",
                    res["availability"].as_str().unwrap_or("Unknown"),
                    res["activity"].as_str().unwrap_or("Unknown")
                );
            }
            Ok(0)
        }
        Some(PresenceAction::Get { person, json }) => {
            let conn = open_optional_archive()?;
            let archive = conn.as_ref().map_or(ArchiveSource::None, ArchiveSource::Conn);
            let res = live.get_presence(archive, &principal, &person)?;
            if json || top_json {
                fmt::print_json(&res);
                return Ok(0);
            }
            if res["self"].as_bool().unwrap_or(false) {
                println!(
                    "{} ({})",
                    res["availability"].as_str().unwrap_or("Unknown"),
                    res["activity"].as_str().unwrap_or("Unknown")
                );
            } else {
                for p in res["presence"].as_array().into_iter().flatten() {
                    println!(
                        "{:<30}  {:<15}  ({})",
                        p["name"].as_str().unwrap_or("?"),
                        p["availability"].as_str().unwrap_or("Unknown"),
                        p["activity"].as_str().unwrap_or("Unknown")
                    );
                }
            }
            Ok(0)
        }
        Some(PresenceAction::Set {
            availability,
            expires,
            flags,
        }) => {
            let res = live.set_presence(&principal, &availability, expires.as_deref(), flags.dry_run)?;
            if flags.json || top_json {
                fmt::print_json(&res);
                return Ok(0);
            }
            if res["dryRun"].as_bool().unwrap_or(false) {
                println!(
                    "[dry run] Would set presence to {}.",
                    res["availability"].as_str().unwrap_or("")
                );
            } else {
                println!("Presence set to {}.", res["availability"].as_str().unwrap_or(""));
            }
            Ok(0)
        }
        Some(PresenceAction::Message { action: ma }) => match ma {
            PresenceMessageAction::Set {
                message,
                expires,
                flags,
            } => {
                let text = message.join(" ");
                let res = live.set_status_message(&principal, Some(&text), false, expires.as_deref(), flags.dry_run)?;
                if flags.json || top_json {
                    fmt::print_json(&res);
                    return Ok(0);
                }
                if res["dryRun"].as_bool().unwrap_or(false) {
                    println!("[dry run] Would set status message to {}.", Quoted(&text));
                } else {
                    println!("Status message set to {}.", Quoted(&text));
                }
                Ok(0)
            }
            PresenceMessageAction::Clear { flags } => {
                let res = live.set_status_message(&principal, None, true, None, flags.dry_run)?;
                if flags.json || top_json {
                    fmt::print_json(&res);
                    return Ok(0);
                }
                if res["dryRun"].as_bool().unwrap_or(false) {
                    println!("[dry run] Would clear status message.");
                } else {
                    println!("Cleared status message.");
                }
                Ok(0)
            }
        },
    }
}

fn run_archive(action: ArchiveAction) -> Result<i32, CliError> {
    match action {
        ArchiveAction::Verify { file, no_backups, json } => {
            let target = file.as_deref().map(paths::expanduser);
            let res = backups::verify_archive(None, target.as_deref(), !no_backups);
            let ok = res["ok"].as_bool().unwrap_or(false);
            if json {
                fmt::print_json(&res);
                return Ok(if ok { 0 } else { 1 });
            }
            let label = res["target"].as_str().or(res["archive"].as_str()).unwrap_or("");
            println!("{}  {label}", if ok { "OK" } else { "FAILED" });
            if let Some(ch) = res["checks"].as_object() {
                for (k, v) in ch {
                    let s = if let Some(b) = v.as_bool() {
                        if b {
                            "ok".to_string()
                        } else {
                            "FAILED".to_string()
                        }
                    } else if let Some(rows) = v.get("rows").and_then(|x| x.as_u64()) {
                        let bad = v.get("bad").and_then(|x| x.as_u64()).unwrap_or(0);
                        if bad > 0 {
                            format!("{rows} versions decode, {bad} do not")
                        } else {
                            format!("{rows} versions decode")
                        }
                    } else {
                        v.to_string()
                    };
                    println!("  {k:<24} {s}");
                }
            }
            if let Some(bks) = res["backups"].as_array().filter(|b| !b.is_empty()) {
                let good = bks.iter().filter(|b| b["ok"].as_bool().unwrap_or(false)).count();
                println!("  backups                  {good} of {} pass quick_check", bks.len());
            }
            for e in res["errors"].as_array().into_iter().flatten() {
                if let Some(s) = e.as_str() {
                    eprintln!("! {s}");
                }
            }
            Ok(if ok { 0 } else { 1 })
        }
        ArchiveAction::Restore {
            backup,
            yes,
            no_safety_backup,
            json,
        } => {
            let bpath = paths::expanduser(&backup);
            if yes {
                let name = bpath.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if !bpath.is_file() {
                    return Err(CliError::Runtime(format!(
                        "{} failed verification: file does not exist",
                        bpath.display()
                    )));
                }
                if !(name.ends_with(".sqlite3") || name.ends_with(".sqlite3.xz")) {
                    return Err(CliError::Runtime(format!(
                        "{} failed verification: not a .sqlite3 or .sqlite3.xz backup",
                        bpath.display()
                    )));
                }
                return restore_now(&bpath, no_safety_backup, json);
            }
            let check = backups::verify_archive(None, Some(&bpath), false);
            if !check["ok"].as_bool().unwrap_or(false) {
                let errs: Vec<&str> = check["errors"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .take(3)
                    .filter_map(|v| v.as_str())
                    .collect();
                return Err(CliError::Runtime(format!(
                    "{} failed verification: {}",
                    bpath.display(),
                    errs.join("; ")
                )));
            }
            let dest = paths::archive_path().to_string_lossy().into_owned();
            if json {
                fmt::print_json(&json!({
                    "dryRun": true,
                    "backup": bpath.to_string_lossy(),
                    "verified": true,
                    "archive": dest,
                }));
                return Ok(0);
            }
            println!("Backup {} verified OK.", bpath.display());
            println!("Restoring will:");
            println!("  1. stop the background daemon, if it is running");
            if !no_safety_backup {
                println!(
                    "  2. save a permanent safety backup of the current archive in {}",
                    paths::backups_dir().display()
                );
            }
            println!(
                "  {}. replace {dest} with the backup, then restart the daemon",
                if !no_safety_backup { 3 } else { 2 }
            );
            println!("Nothing has changed yet. Re-run with --yes to proceed.");
            Ok(0)
        }
        ArchiveAction::Backup { out } => {
            let conn = archive::connect(None)?;
            let dest = if let Some(o) = out {
                backups::backup(&conn, &paths::expanduser(&o))?
            } else {
                let _lock = sync::acquire_sync_lock(120.0).map_err(|e| CliError::Runtime(e.to_string()))?;
                backups::backup_if_due(&conn, 0.0)?.ok_or_else(|| CliError::Runtime("Backup skipped".to_string()))?
            };
            println!("Backup written: {}", dest.display());
            Ok(0)
        }
        ArchiveAction::Reindex => {
            let _lock = sync::acquire_sync_lock(120.0).map_err(|e| CliError::Runtime(e.to_string()))?;
            let conn = archive::connect(None)?;
            let n = archive::rerender(&conn, true)?;
            println!("Re-rendered {n} messages from stored HTML.");
            Ok(0)
        }
        ArchiveAction::Migrations { json } => {
            let conn = api::open_archive_readonly()?;
            let current: i64 = conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .map_err(|e| CliError::Runtime(e.to_string()))?;
            let applied = migrations::history(&conn)?;
            let pending = migrations::pending(&conn)?;
            if json {
                fmt::print_json(&json!({
                    "currentVersion": current,
                    "targetVersion": archive::SCHEMA_VERSION,
                    "applied": applied.iter().map(|m| json!({
                        "version": m.version,
                        "name": m.name,
                        "appliedAt": iso((m.applied_at != 0).then_some(m.applied_at)),
                        "appliedAtMs": m.applied_at,
                        "appVersion": m.app_version,
                        "durationMs": m.duration_ms,
                    })).collect::<Vec<_>>(),
                    "pending": pending.iter().map(|m| json!({
                        "version": m.version,
                        "name": m.name,
                        "description": m.description,
                    })).collect::<Vec<_>>(),
                }));
                return Ok(0);
            }
            println!("Schema version: v{current} (target v{})", archive::SCHEMA_VERSION);
            for m in &applied {
                let when = if m.applied_at == 0 {
                    "backfill".to_string()
                } else {
                    short(Some(m.applied_at))
                };
                println!(
                    "  v{:<3} {:<44} {:<16} app={} ({}ms)",
                    m.version, m.name, when, m.app_version, m.duration_ms
                );
            }
            if !pending.is_empty() {
                println!("\nPending:");
                for m in &pending {
                    println!("  v{:<3} {:<44} {}", m.version, m.name, m.description);
                }
            }
            Ok(0)
        }
    }
}

fn run_daemon(action: DaemonAction) -> Result<i32, CliError> {
    match action {
        DaemonAction::Install {
            interval,
            port,
            allow_writes,
        } => {
            if allow_writes {
                session::preflight_keychain().map_err(CliError::Runtime)?;
            }
            let path = launchd::install(interval, port, allow_writes).map_err(CliError::Runtime)?;
            println!(
                "Installed {}\nThe daemon checks for updates every {} and serves http://127.0.0.1:{} (writes={allow_writes})",
                path.display(),
                interval_label(interval),
                port.unwrap_or_else(paths::port)
            );
            println!("Logs: {}", paths::log_dir().display());
            Ok(0)
        }
        DaemonAction::Uninstall => {
            println!(
                "{}",
                if launchd::uninstall() {
                    "Removed."
                } else {
                    "Not installed."
                }
            );
            Ok(0)
        }
        DaemonAction::Restart => {
            let path = launchd::restart().map_err(CliError::Runtime)?;
            println!("Restarted {}.", path.display());
            Ok(0)
        }
        DaemonAction::Status => {
            fmt::print_json(&launchd::status());
            Ok(0)
        }
        DaemonAction::Logs { lines, follow } => {
            let log_path = paths::log_dir().join("daemon.log");
            tail_daemon_log(&log_path, lines, follow).map_err(CliError::Runtime)?;
            Ok(0)
        }
        DaemonAction::Run {
            port,
            interval,
            no_sync,
            allow_writes,
            log_stderr,
        } => {
            daemon::serve(port, interval, !no_sync, allow_writes, !log_stderr).map_err(CliError::Runtime)?;
            Ok(0)
        }
    }
}

fn run_token(action: TokenAction) -> Result<i32, CliError> {
    match action {
        TokenAction::Create {
            name,
            scope,
            chats,
            expires,
            json,
        } => {
            let mut parsed_scopes = Vec::new();
            for s in &scope {
                let parsed = Scope::parse(s).map_err(|e| CliError::Api(api::ApiError::bad("bad_param", e)))?;
                parsed_scopes.push(parsed);
            }
            if parsed_scopes.is_empty() {
                parsed_scopes.push(Scope::Read);
            }
            let allow_chats = if chats.is_empty() {
                None
            } else {
                if !parsed_scopes.iter().any(Scope::is_chat_targeted) {
                    return Err(CliError::Api(api::ApiError::bad(
                        "bad_param",
                        "`--chats` restricts live chat actions (`send`, `modify`, `react`, `mark-read`, `chat:manage`); archive reads are not chat-filtered.",
                    )));
                }
                let conn = open_optional_archive()?;
                let mut ids = Vec::with_capacity(chats.len());
                for ch in &chats {
                    let (resolved, _) = resolve::resolve_chat(conn.as_ref(), ch, Strictness::Strict)
                        .map_err(|e| CliError::Live(LiveError::from(e)))?;
                    ids.push(resolved.id);
                }
                Some(ids)
            };
            let res = daemon::create_named_token(&name, expires.as_deref(), &parsed_scopes, allow_chats)
                .map_err(CliError::Runtime)?;
            if json {
                fmt::print_json(&res);
            } else {
                println!("{}", res["token"].as_str().unwrap_or(""));
                let exp = res["expiresAt"].as_str().unwrap_or("never");
                let scopes_s: Vec<&str> = res["scopes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                let chats_s: Vec<&str> = res["allowChats"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                let chats_note = if chats_s.is_empty() {
                    String::new()
                } else {
                    format!(", chats: {}", chats_s.join(","))
                };
                eprintln!(
                    "# Created token {} (id {}, scopes: {}{}, expires: {}). Save it now; only its SHA-256 hash is stored.",
                    Quoted(res["name"].as_str().unwrap_or("")),
                    res["id"].as_str().unwrap_or(""),
                    scopes_s.join(","),
                    chats_note,
                    exp
                );
            }
            Ok(0)
        }
        TokenAction::List { include_expired, json } => {
            let res = daemon::list_named_tokens(include_expired).map_err(CliError::Runtime)?;
            if json {
                fmt::print_json(&res);
                return Ok(0);
            }
            let tokens = res["tokens"].as_array().cloned().unwrap_or_default();
            if tokens.is_empty() {
                println!("No named tokens. Create one with `teams token create <name>`.");
                return Ok(0);
            }
            for t in &tokens {
                let exp = t["expiresAt"].as_str().unwrap_or("never");
                let state = if t["expired"].as_bool().unwrap_or(false) {
                    "expired"
                } else {
                    "active"
                };
                let scopes_s: Vec<&str> = t["scopes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                let chats_s: Vec<&str> = t["allowChats"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                let chats_col = if chats_s.is_empty() {
                    String::new()
                } else {
                    format!("  chats={}", chats_s.join(","))
                };
                println!(
                    "{}  {:<18}  {:<12}  {:<7}  scopes={:<24}  expires {}{}",
                    t["id"].as_str().unwrap_or(""),
                    t["name"].as_str().unwrap_or(""),
                    t["prefix"].as_str().unwrap_or(""),
                    state,
                    scopes_s.join(","),
                    exp,
                    chats_col
                );
            }
            Ok(0)
        }
        TokenAction::Revoke { target, json } => {
            let res = daemon::revoke_named_token(&target).map_err(CliError::Runtime)?;
            if json {
                fmt::print_json(&res);
            } else {
                println!(
                    "Revoked token {} (id {}).",
                    Quoted(res["name"].as_str().unwrap_or("")),
                    res["id"].as_str().unwrap_or("")
                );
            }
            Ok(0)
        }
        TokenAction::Prune { json } => {
            let res = daemon::prune_expired_tokens().map_err(CliError::Runtime)?;
            if json {
                fmt::print_json(&res);
            } else {
                println!(
                    "Pruned {} expired token(s) ({} remaining).",
                    res["pruned"], res["remaining"]
                );
            }
            Ok(0)
        }
    }
}

fn run_debug(action: DebugAction) -> Result<i32, CliError> {
    match action {
        DebugAction::Records {
            source,
            key,
            since,
            all_versions,
            limit,
            json,
        } => {
            let conn = api::open_archive_readonly()?;
            match source {
                None => {
                    let r = api::record_sources(&conn)?;
                    if json {
                        fmt::print_json(&r);
                    } else if let Some(srcs) = r["sources"].as_array() {
                        for s in srcs {
                            println!(
                                "{:<58} {:>6} records {:>7} versions  last {}",
                                s["source"].as_str().unwrap_or(""),
                                s["records"].as_i64().unwrap_or(0),
                                s["versions"].as_i64().unwrap_or(0),
                                s["lastCaptured"].as_str().unwrap_or("-")
                            );
                        }
                    }
                    Ok(0)
                }
                Some(src) => {
                    let r = api::records(&conn, &src, key.as_deref(), since.as_deref(), limit, all_versions)?;
                    if let Some(recs) = r["records"].as_array() {
                        for rec in recs {
                            println!("{}", json_codec::to_string(rec));
                        }
                    }
                    Ok(0)
                }
            }
        }
        DebugAction::Stores => {
            let cache = TeamsCache::open::<fn() -> Result<(), crate::ingest::SourceError>>(None, None)
                .map_err(|e| CliError::Runtime(e.to_string()))?;
            for name in cache.database_names() {
                println!("{name}\n    {}", cache.stores(&name).join(", "));
            }
            Ok(0)
        }
        DebugAction::Dump { db, store } => {
            let mut cache = TeamsCache::open::<fn() -> Result<(), crate::ingest::SourceError>>(None, None)
                .map_err(|e| CliError::Runtime(e.to_string()))?;
            let matches: Vec<String> = cache.database_names().into_iter().filter(|n| n.contains(&db)).collect();
            let Some(first_db) = matches.first() else {
                eprintln!("No database name contains {}", Quoted(&db));
                return Ok(2);
            };
            let stores = cache.stores(first_db);
            if !stores.iter().any(|s| s == &store) {
                let avail = if stores.is_empty() {
                    "(none)".to_string()
                } else {
                    stores.join(", ")
                };
                eprintln!("No store {} in {first_db}. Stores: {avail}", Quoted(&store));
                return Ok(2);
            }
            for (k, v) in cache.raw_records_with_keys(first_db, &store) {
                println!("{}", json_codec::to_string(&json!({"key": k, "value": v})));
            }
            Ok(0)
        }
    }
}

fn restore_now(bpath: &std::path::Path, no_safety_backup: bool, json: bool) -> Result<i32, CliError> {
    let manages_default_home = std::env::var_os("TEAMS_CLI_HOME").filter(|v| !v.is_empty()).is_none();
    let was_loaded = if manages_default_home {
        let st = launchd::status();
        st["installed"].as_bool().unwrap_or(false) && st["loaded"].as_bool().unwrap_or(false)
    } else {
        false
    };
    if was_loaded && !launchd::stop(20.0) {
        return Err(CliError::Runtime(
            "Could not stop the running daemon; run `teams daemon uninstall` and retry.".to_string(),
        ));
    }
    let res_inner = (|| -> Result<Value, CliError> {
        let _lock = sync::acquire_sync_lock(120.0).map_err(|e| CliError::Runtime(e.to_string()))?;
        let r = backups::restore_backup(bpath, None, !no_safety_backup).map_err(|e| restore_error(bpath, e))?;
        Ok(r)
    })();
    let restart_err = if was_loaded { launchd::start().err() } else { None };
    let mut res = res_inner?;
    res["daemonRestarted"] = json!(was_loaded && restart_err.is_none());
    if let Some(ref re) = restart_err {
        res["daemonRestartError"] = json!(re);
    }
    if json {
        fmt::print_json(&res);
    } else {
        println!(
            "Restored {} from {}: {} messages in {} chats.",
            res["archive"].as_str().unwrap_or(""),
            res["restoredFrom"].as_str().unwrap_or(""),
            res["messages"],
            res["chats"]
        );
        println!(
            "Safety backup: {}",
            res["safetyBackup"].as_str().unwrap_or("none (nothing to keep)")
        );
        if res["daemonRestarted"].as_bool().unwrap_or(false) {
            println!("Daemon restarted.");
        }
    }
    if let Some(re) = restart_err {
        eprintln!("teams: daemon restart failed ({re}); run `teams daemon install`");
        return Ok(1);
    }
    Ok(0)
}

fn restore_error(bpath: &std::path::Path, e: archive::ArchiveError) -> CliError {
    let msg = e.to_string();
    if msg.contains("left untouched")
        || msg.contains("Could not read")
        || msg.contains("does not exist")
        || msg.contains("is not a .sqlite3")
    {
        CliError::Runtime(format!("{} failed verification: {msg}", bpath.display()))
    } else {
        CliError::Runtime(msg)
    }
}

pub fn interval_label(seconds: u64) -> String {
    if seconds >= 60 && seconds.is_multiple_of(60) {
        format!("{} min", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

fn tail_daemon_log(log_path: &std::path::Path, lines: usize, follow: bool) -> Result<(), String> {
    use std::io::{Seek, SeekFrom, Write};
    use std::os::unix::fs::MetadataExt;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    let text = std::fs::read_to_string(log_path).map_err(|e| format!("Could not read {}: {e}", log_path.display()))?;
    let all: Vec<&str> = text.lines().collect();
    let start = if lines == 0 { 0 } else { all.len().saturating_sub(lines) };
    let mut stdout = std::io::stdout().lock();
    for line in &all[start..] {
        if writeln!(stdout, "{line}").is_err() {
            return Ok(());
        }
    }
    let _ = stdout.flush();
    if !follow {
        return Ok(());
    }

    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop);
    let _ = ctrlc::set_handler(move || {
        stop_flag.store(true, Ordering::SeqCst);
    });

    let mut pos = text.len() as u64;
    let mut ino = std::fs::metadata(log_path).map(|m| m.ino()).unwrap_or(0);
    let mut buf = Vec::new();
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(250));
        let Ok(meta) = std::fs::metadata(log_path) else {
            continue;
        };
        let cur_ino = meta.ino();
        let cur_len = meta.len();
        if cur_ino != ino || cur_len < pos {
            ino = cur_ino;
            pos = 0;
        }
        if cur_len == pos {
            continue;
        }
        let Ok(mut f) = std::fs::File::open(log_path) else {
            continue;
        };
        if f.seek(SeekFrom::Start(pos)).is_err() {
            continue;
        }
        buf.clear();
        if f.read_to_end(&mut buf).is_ok() && !buf.is_empty() {
            pos += buf.len() as u64;
            if stdout.write_all(&buf).is_err() || stdout.flush().is_err() {
                break;
            }
        }
    }
    Ok(())
}

pub fn docs_text() -> String {
    include_str!("docs.txt")
        .replacen("{version}", VERSION, 1)
        .replace("{port}", &paths::port().to_string())
}
