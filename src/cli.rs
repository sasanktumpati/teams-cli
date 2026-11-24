use crate::daemon;
use crate::doctor;
use crate::formatters::{self as fmt, Quoted};
use crate::ingest::{indexeddb::TeamsCache, sync};
use crate::json_codec;
use crate::launchd;
use crate::live::{fetch, send};
use crate::paths;
use crate::query::{api, export, views};
use crate::storage::{archive, backups, migrations};
use crate::timeutil::{iso, short};
use crate::update;
use clap::{CommandFactory, Parser, Subcommand};
use serde_json::{json, Value};
use std::io::Read;
use std::path::PathBuf;

pub const VERSION: &str = crate::VERSION;
pub const MIN_SYNC_INTERVAL: u64 = 10;

#[derive(Parser)]
#[command(
    name = "teams",
    display_name = "teams-cli",
    version = VERSION,
    about = "Durable local archive of your Microsoft Teams chats."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    #[command(about = "Everything on one page: commands, flags, JSON shapes (for agents and scripts)")]
    Docs,

    #[command(about = "Copy new messages from the Teams cache into the archive")]
    Sync {
        #[arg(long, help = "Override the Teams IndexedDB directory path")]
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

    #[command(about = "Show archive schema version, applied migration ledger, and any pending upgrades")]
    Migrations {
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },

    #[command(about = "List chats, channels and meetings by recent activity")]
    Chats {
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

    #[command(about = "Read a chat (by name or id), or the messages around one message")]
    Messages {
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
        #[arg(long, help = "Only messages at or after this time (e.g. 7d, today, 2026-11-01)")]
        since: Option<String>,
        #[arg(long, help = "Only messages before this time")]
        until: Option<String>,
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
        include_history: bool,
        #[arg(long, help = "Return only the match count")]
        count: bool,
        #[arg(long, help = "Only messages that have at least one file, image, or link attachment")]
        has_attachments: bool,
        #[arg(long, help = "Only messages at or after this time")]
        since: Option<String>,
        #[arg(long, help = "Only messages before this time")]
        until: Option<String>,
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

    #[command(about = "Files, images and links shared in messages, with their links")]
    Attachments {
        #[arg(help = "Filter by file name or URL substring")]
        query: Vec<String>,
        #[arg(long, help = "Limit to one chat (name fragment or id)")]
        chat: Option<String>,
        #[arg(long, value_parser = ["file", "image", "link"], help = "Filter by attachment kind")]
        kind: Option<String>,
        #[arg(long = "type", help = "Filter files by extension (e.g. pdf, xlsx, zip)")]
        file_type: Option<String>,
        #[arg(long, help = "Only attachments shared at or after this time")]
        since: Option<String>,
        #[arg(long, help = "Only attachments shared before this time")]
        until: Option<String>,
        #[arg(long = "from", help = "Limit to one sender (name, email, or id)")]
        sender: Option<String>,
        #[arg(
            long,
            default_value_t = 50,
            value_parser = nonneg_int,
            allow_negative_numbers = true,
            help = "Maximum attachments to return (0 = all)"
        )]
        limit: usize,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },

    #[command(about = "Download a shared file or pasted image on demand (CLI only)")]
    Fetch {
        #[arg(help = "Attachment URL, message id, or filename query")]
        target: Vec<String>,
        #[arg(long, help = "Message id (or chatId/msgId) owning the attachment")]
        message: Option<String>,
        #[arg(long, help = "1-based attachment index when a message or query has multiple matches")]
        index: Option<usize>,
        #[arg(long, help = "Destination file or directory path")]
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

    #[command(about = "[RESTRICTED] Send a Teams message (CLI only; dry run unless --yes & TEAMS_CLI_ENABLE_SEND=1)")]
    Send {
        #[arg(help = "Target conversation id or unambiguous chat name")]
        target: String,
        #[arg(help = "Message body text")]
        message: Vec<String>,
        #[arg(long, help = "Read message body from stdin")]
        stdin: bool,
        #[arg(long, help = "Reply to a numeric channel thread root message id")]
        reply_to: Option<String>,
        #[arg(long, help = "Send body as raw HTML instead of escaping plain text")]
        html: bool,
        #[arg(long, help = "Explicit numeric client message id (1–32 digits)")]
        client_message_id: Option<String>,
        #[arg(long, help = "Preview the request without sending")]
        dry_run: bool,
        #[arg(long, help = "Actually send the message (also requires TEAMS_CLI_ENABLE_SEND=1)")]
        yes: bool,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },

    #[command(about = "People seen in Teams")]
    People {
        #[arg(long, help = "Filter by name, email, or job title substring")]
        grep: Option<String>,
        #[arg(long, help = "Include bots")]
        bots: bool,
        #[arg(
            long,
            default_value_t = 50,
            value_parser = nonneg_int,
            allow_negative_numbers = true,
            help = "Maximum people to return (0 = all)"
        )]
        limit: usize,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },

    #[command(about = "One person: profile, first/last contact, shared chats, recent files")]
    Person {
        #[arg(required = true, help = "Name fragment, email, or Teams id")]
        person: Vec<String>,
        #[arg(
            long,
            default_value_t = 20,
            value_parser = nonneg_int,
            allow_negative_numbers = true,
            help = "Maximum shared chats and recent files to return (0 = all)"
        )]
        limit: usize,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },

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

    #[command(about = "Messages that @mention you, newest first")]
    Mentions {
        #[arg(long, help = "Only mentions at or after this time")]
        since: Option<String>,
        #[arg(long, help = "Only mentions before this time")]
        until: Option<String>,
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

    #[command(about = "Call history with direction, state, duration and participants")]
    Calls {
        #[arg(long, help = "Only calls at or after this time")]
        since: Option<String>,
        #[arg(long, help = "Only calls before this time")]
        until: Option<String>,
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
        #[arg(long, help = "Only meetings ending at or after this time")]
        since: Option<String>,
        #[arg(long, help = "Only meetings starting before this time")]
        until: Option<String>,
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

    #[command(about = "Messages you saved (bookmarked), most recently saved first")]
    Saved {
        #[arg(long, help = "Include messages you later unsaved")]
        all: bool,
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

    #[command(about = "A chat as Markdown (whole history by default)")]
    Export {
        #[arg(help = "Chat name fragment or conversation id")]
        chat: String,
        #[arg(long, help = "Only messages at or after this time")]
        since: Option<String>,
        #[arg(long, help = "Only messages before this time")]
        until: Option<String>,
        #[arg(long = "from", help = "Only messages from this sender")]
        sender: Option<String>,
        #[arg(long, help = "Export a single channel thread root message id")]
        thread: Option<String>,
        #[arg(long, help = "Include system events")]
        events: bool,
        #[arg(long, help = "Include earlier versions under edited messages")]
        history: bool,
        #[arg(long, help = "Write Markdown to this file (mode 0600) instead of stdout")]
        out: Option<String>,
    },

    #[command(about = "Prove the archive (or one backup FILE) reads end to end; exit 1 if not")]
    Verify {
        #[arg(help = "Specific `.sqlite3` or `.sqlite3.xz` backup file to verify")]
        file: Option<String>,
        #[arg(long, help = "Skip verifying files in the backups directory")]
        no_backups: bool,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },

    #[command(about = "Replace the archive with a verified backup (dry run unless --yes)")]
    Restore {
        #[arg(help = "Path to the `.sqlite3` or `.sqlite3.xz` backup file")]
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

    #[command(about = "Write a consistent copy of the archive")]
    Backup {
        #[arg(
            long,
            help = "Destination path (`.sqlite3` or `.sqlite3.xz`); omit for scheduled backup directory"
        )]
        out: Option<String>,
    },

    #[command(about = "Re-render every archived message with the current renderer")]
    Reindex,

    #[command(about = "Run the sync loop and local HTTP API")]
    Daemon {
        #[arg(long, help = "Loopback HTTP port (default 47613 or TEAMS_CLI_PORT)")]
        port: Option<u16>,
        #[arg(
            long,
            default_value_t = paths::DEFAULT_SYNC_INTERVAL,
            value_parser = min_interval,
            help = "Seconds between lightweight cache refresh checks (minimum 10)"
        )]
        interval: u64,
        #[arg(long, help = "Serve the read-only HTTP API without periodic syncing")]
        no_sync: bool,
        #[arg(
            long = "log-stderr",
            alias = "foreground",
            help = "Write daemon logs to stderr instead of daemon.log"
        )]
        log_stderr: bool,
    },

    #[command(about = "Manage HTTP API bearer tokens (default: print or --rotate primary token)")]
    Token {
        #[command(subcommand)]
        action: Option<TokenAction>,
        #[arg(
            long,
            help = "Replace the primary token in api.token (effective immediately, no restart)"
        )]
        rotate: bool,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },

    #[command(about = "Install, restart, inspect, or view logs for the background daemon (launchd)")]
    Agent {
        #[command(subcommand)]
        action: AgentAction,
    },

    #[command(about = "Debug: inspect the live Teams cache directly")]
    Raw {
        #[command(subcommand)]
        action: RawAction,
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
    },
}

#[derive(Subcommand)]
pub enum TokenAction {
    #[command(about = "Create a named bearer token (stored SHA-256 hashed; secret printed once)")]
    Create {
        #[arg(help = "Human-readable label for the token (e.g. executor, ci, script)")]
        name: String,
        #[arg(long, help = "Optional expiry (e.g. 12h, 7d, 30d, 1y, 2027-01-01, or never)")]
        expires: Option<String>,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },
    #[command(about = "List named bearer tokens (metadata only; secrets are never shown)")]
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
pub enum AgentAction {
    #[command(about = "Install and start the launchd background agent")]
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
    },
    #[command(about = "Stop and remove the launchd background agent")]
    Uninstall,
    #[command(about = "Restart the installed launchd background agent")]
    Restart,
    #[command(about = "Show launchd agent state as JSON")]
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
    },
}

#[derive(Subcommand)]
pub enum RawAction {
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

pub enum CliError {
    Api(api::ApiError),
    Runtime(String),
}

impl From<api::ApiError> for CliError {
    fn from(e: api::ApiError) -> Self {
        CliError::Api(e)
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

pub fn run(cli: Cli) -> Result<i32, CliError> {
    match cli.command {
        Commands::Docs => {
            print!("{}", docs_text());
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
                println!("Daemon    not installed - run `teams agent install`");
            }
            println!(
                "Backups   {} (latest: {})",
                s["backups"]["count"],
                s["backups"]["latest"].as_str().unwrap_or("none")
            );
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
        Commands::Migrations { json } => {
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
        Commands::Chats {
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
        Commands::Messages {
            chat,
            around,
            context,
            thread,
            history,
            since,
            until,
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
                since.as_deref(),
                until.as_deref(),
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
        Commands::Search {
            query,
            chat,
            relevance,
            include_history,
            count,
            has_attachments,
            since,
            until,
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
                since.as_deref(),
                until.as_deref(),
                limit,
                if relevance { "relevance" } else { "recent" },
                events,
                include_history,
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
        Commands::Attachments {
            query,
            chat,
            kind,
            file_type,
            since,
            until,
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
                since.as_deref(),
                until.as_deref(),
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
            println!("\n# {} of {} attachments{hint}", atts.len(), r["matched"]);
            fmt::note_senders(r.get("senders"), sender.as_deref());
            Ok(0)
        }
        Commands::Fetch {
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
                if paths::archive_path().exists() {
                    Some(api::open_archive_readonly()?)
                } else {
                    None
                }
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
        Commands::Send {
            target,
            message,
            stdin,
            reply_to,
            html,
            client_message_id,
            dry_run,
            yes,
            json,
        } => {
            let text = if stdin {
                let mut buf = String::new();
                std::io::stdin()
                    .read_to_string(&mut buf)
                    .map_err(|e| CliError::Runtime(e.to_string()))?;
                buf
            } else {
                message.join(" ")
            };
            if text.trim().is_empty() {
                return Err(CliError::Api(api::ApiError::bad(
                    "bad_param",
                    "Provide message text or pass --stdin.",
                )));
            }
            let effective_dry_run = dry_run || !yes;
            let conn = if paths::archive_path().exists() {
                Some(api::open_archive_readonly()?)
            } else {
                None
            };
            let res = send::send_message(
                conn.as_ref(),
                &target,
                &text,
                reply_to.as_deref(),
                html,
                client_message_id.as_deref(),
                effective_dry_run,
            )
            .map_err(|e| CliError::Runtime(e.to_string()))?;
            if json {
                fmt::print_json(&res);
                return Ok(0);
            }
            if res["dryRun"].as_bool().unwrap_or(false) {
                let status = if res["restricted"].as_bool().unwrap_or(true) {
                    "RESTRICTED (set TEAMS_CLI_ENABLE_SEND=1 to enable)"
                } else {
                    "enabled"
                };
                let allow = if res["allowed"].as_bool().unwrap_or(true) {
                    ""
                } else {
                    "  [BLOCKED by TEAMS_CLI_SEND_ALLOWLIST]"
                };
                println!("[RESTRICTED — dry run; sending is {status}]{allow}");
                println!(
                    "  To:      {}  ({}, {})",
                    res["chatName"].as_str().unwrap_or(""),
                    res["chatKind"].as_str().unwrap_or(""),
                    res["conversationId"].as_str().unwrap_or("")
                );
                if let Some(rt) = res["replyTo"].as_str() {
                    println!("  Reply:   thread {rt}");
                }
                println!("  Text:    {}", res["text"].as_str().unwrap_or(""));
                println!("Nothing was sent. Re-run with TEAMS_CLI_ENABLE_SEND=1 and --yes to send.");
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
        Commands::Records {
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
        Commands::People {
            grep,
            bots,
            limit,
            json,
        } => {
            let conn = api::open_archive_readonly()?;
            api::require_data(&conn)?;
            let r = api::people(&conn, grep.as_deref(), limit, bots)?;
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
        Commands::Person { person, limit, json } => {
            let conn = api::open_archive_readonly()?;
            api::require_data(&conn)?;
            let r = api::person(&conn, &person.join(" "), limit)?;
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
            since,
            until,
            chat,
            unread,
            limit,
            json,
        } => {
            let conn = api::open_archive_readonly()?;
            api::require_data(&conn)?;
            let r = views::mentions(
                &conn,
                since.as_deref(),
                until.as_deref(),
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
            since,
            until,
            person,
            missed,
            limit,
            json,
        } => {
            let conn = api::open_archive_readonly()?;
            api::require_data(&conn)?;
            let r = views::calls(
                &conn,
                since.as_deref(),
                until.as_deref(),
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
            since,
            until,
            grep,
            limit,
            json,
        } => {
            let conn = api::open_archive_readonly()?;
            api::require_data(&conn)?;
            let r = views::meetings(&conn, since.as_deref(), until.as_deref(), grep.as_deref(), limit)?;
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
        Commands::Saved { all, limit, json } => {
            let conn = api::open_archive_readonly()?;
            api::require_data(&conn)?;
            let r = views::saved(&conn, all, limit)?;
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
        Commands::Pinned { chat, limit, json } => {
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
        Commands::Export {
            chat,
            since,
            until,
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
                since.as_deref(),
                until.as_deref(),
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
        Commands::Verify { file, no_backups, json } => {
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
        Commands::Restore {
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
        Commands::Backup { out } => {
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
        Commands::Reindex => {
            let _lock = sync::acquire_sync_lock(120.0).map_err(|e| CliError::Runtime(e.to_string()))?;
            let conn = archive::connect(None)?;
            let n = archive::rerender(&conn, true)?;
            println!("Re-rendered {n} messages from stored HTML.");
            Ok(0)
        }
        Commands::Daemon {
            port,
            interval,
            no_sync,
            log_stderr,
        } => {
            daemon::serve(port, interval, !no_sync, !log_stderr).map_err(CliError::Runtime)?;
            Ok(0)
        }
        Commands::Token { action, rotate, json } => match action {
            None => {
                let env_token = daemon::env_api_token();
                let token = if rotate {
                    let t = daemon::rotate_api_token().map_err(CliError::Runtime)?;
                    if env_token.is_some() {
                        eprintln!(
                            "teams: note: {} is set and overrides {}; the daemon keeps using the environment token",
                            daemon::API_TOKEN_ENV,
                            paths::api_token_path().display()
                        );
                    }
                    t
                } else {
                    daemon::load_or_create_api_token().map_err(CliError::Runtime)?
                };
                let (shown, source) = match env_token {
                    Some(t) if !rotate => (t, "env"),
                    _ => (token, "file"),
                };
                if json {
                    fmt::print_json(&json!({
                        "token": shown,
                        "path": paths::api_token_path().to_string_lossy(),
                        "source": source,
                    }));
                } else {
                    println!("{shown}");
                }
                Ok(0)
            }
            Some(TokenAction::Create {
                name,
                expires,
                json: sub_json,
            }) => {
                let res = daemon::create_named_token(&name, expires.as_deref()).map_err(CliError::Runtime)?;
                if daemon::env_api_token().is_some() {
                    eprintln!(
                        "teams: note: {} is set and overrides token files while active",
                        daemon::API_TOKEN_ENV
                    );
                }
                if json || sub_json {
                    fmt::print_json(&res);
                } else {
                    println!("{}", res["token"].as_str().unwrap_or(""));
                    let exp = res["expiresAt"].as_str().unwrap_or("never");
                    eprintln!(
                        "# Created token {} (id {}, expires: {}). Save it now; only its SHA-256 hash is stored.",
                        Quoted(res["name"].as_str().unwrap_or("")),
                        res["id"].as_str().unwrap_or(""),
                        exp
                    );
                }
                Ok(0)
            }
            Some(TokenAction::List {
                include_expired,
                json: sub_json,
            }) => {
                let res = daemon::list_named_tokens(include_expired).map_err(CliError::Runtime)?;
                if json || sub_json {
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
                    println!(
                        "{}  {:<20}  {:<12}  {:<7}  expires {}",
                        t["id"].as_str().unwrap_or(""),
                        t["name"].as_str().unwrap_or(""),
                        t["prefix"].as_str().unwrap_or(""),
                        state,
                        exp
                    );
                }
                Ok(0)
            }
            Some(TokenAction::Revoke { target, json: sub_json }) => {
                let res = daemon::revoke_named_token(&target).map_err(CliError::Runtime)?;
                if json || sub_json {
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
            Some(TokenAction::Prune { json: sub_json }) => {
                let res = daemon::prune_expired_tokens().map_err(CliError::Runtime)?;
                if json || sub_json {
                    fmt::print_json(&res);
                } else {
                    println!(
                        "Pruned {} expired token(s) ({} remaining).",
                        res["pruned"], res["remaining"]
                    );
                }
                Ok(0)
            }
        },
        Commands::Agent { action } => match action {
            AgentAction::Install { interval, port } => {
                let path = launchd::install(interval, port).map_err(CliError::Runtime)?;
                println!(
                    "Installed {}\nThe daemon checks for updates every {} and serves http://127.0.0.1:{}",
                    path.display(),
                    interval_label(interval),
                    port.unwrap_or_else(paths::port)
                );
                println!("Logs: {}", paths::log_dir().display());
                Ok(0)
            }
            AgentAction::Uninstall => {
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
            AgentAction::Restart => {
                let path = launchd::restart().map_err(CliError::Runtime)?;
                println!("Restarted {}.", path.display());
                Ok(0)
            }
            AgentAction::Status => {
                fmt::print_json(&launchd::status());
                Ok(0)
            }
            AgentAction::Logs { lines } => {
                let log_path = paths::log_dir().join("daemon.log");
                let text = std::fs::read_to_string(&log_path)
                    .map_err(|e| CliError::Runtime(format!("Could not read {}: {e}", log_path.display())))?;
                let all: Vec<&str> = text.lines().collect();
                let start = if lines == 0 { 0 } else { all.len().saturating_sub(lines) };
                for line in &all[start..] {
                    println!("{line}");
                }
                Ok(0)
            }
        },
        Commands::Raw { action } => {
            let mut cache = TeamsCache::open::<fn() -> Result<(), crate::ingest::SourceError>>(None, None)
                .map_err(|e| CliError::Runtime(e.to_string()))?;
            match action {
                RawAction::Stores => {
                    for name in cache.database_names() {
                        println!("{name}\n    {}", cache.stores(&name).join(", "));
                    }
                    Ok(0)
                }
                RawAction::Dump { db, store } => {
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
        Commands::Completions { shell } => {
            let mut cmd = Cli::command();
            clap_complete::generate(shell, &mut cmd, "teams", &mut std::io::stdout());
            Ok(0)
        }
        Commands::Update {
            check,
            target_version,
            force,
            json,
        } => {
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
                        let short = &hash[..hash.len().min(12)];
                        println!("  {green}{:<12}{reset} sha256 {short}...", "verified");
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
                eprintln!("teams: daemon restart failed ({err}); run `teams agent restart`");
                return Ok(1);
            }
            Ok(0)
        }
    }
}

fn restore_now(bpath: &std::path::Path, no_safety_backup: bool, json: bool) -> Result<i32, CliError> {
    let was_loaded = launchd::status()["loaded"].as_bool().unwrap_or(false);
    if was_loaded && !launchd::stop(20.0) {
        return Err(CliError::Runtime(
            "Could not stop the running daemon; run `teams agent uninstall` and retry.".to_string(),
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
        eprintln!("teams: daemon restart failed ({re}); run `teams agent install`");
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

pub fn docs_text() -> String {
    include_str!("docs.txt")
        .replacen("{version}", VERSION, 1)
        .replace("{port}", &paths::port().to_string())
}
