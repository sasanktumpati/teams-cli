mod common;

use clap::CommandFactory;
use common::*;
use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

struct Out {
    code: i32,
    out: String,
    err: String,
}

fn teams(home: &Path, args: &[&str]) -> Out {
    teams_env(home, args, &[])
}

fn teams_env(home: &Path, args: &[&str], env: &[(&str, &str)]) -> Out {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_teams"));
    cmd.args(args)
        .env("TEAMS_CLI_HOME", home)
        .env_remove("TEAMS_CLI_ENABLE_SEND")
        .env_remove("TEAMS_CLI_SEND_ALLOWLIST")
        .env("TEAMS_CLI_SOURCE", home.join("no-teams-cache"))
        .env("HOME", home)
        .env("TZ", "UTC");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let o = cmd.output().unwrap();
    Out {
        code: o.status.code().unwrap_or(-1),
        out: String::from_utf8(o.stdout).unwrap(),
        err: String::from_utf8(o.stderr).unwrap(),
    }
}

fn teams_json(home: &Path, args: &[&str]) -> Value {
    let mut a = args.to_vec();
    a.push("--json");
    let o = teams(home, &a);
    assert_eq!(o.code, 0, "{args:?}: {}", o.err);
    serde_json::from_str(&o.out).unwrap_or_else(|e| panic!("{args:?}: {e}: {}", o.out))
}

fn home() -> (tempfile::TempDir, std::path::PathBuf) {
    let _g = env_lock();
    home_with_archive()
}

#[test]
fn docs_lists_every_command_and_flag() {
    let td = tempfile::tempdir().unwrap();
    let o = teams(td.path(), &["docs"]);
    assert_eq!(o.code, 0);
    let text = o.out;
    let cmd = teams_cli::cli::Cli::command();
    for sub in cmd.get_subcommands() {
        let name = sub.get_name();
        if name == "docs" || name == "help" || sub.is_hide_set() {
            continue;
        }
        assert!(
            text.contains(&format!("teams {name} ")) || text.contains(&format!("teams {name}\n")),
            "{name}"
        );
        let mut subs: Vec<&clap::Command> = vec![sub];
        subs.extend(sub.get_subcommands());
        for s in subs {
            for arg in s.get_arguments() {
                if let Some(long) = arg.get_long() {
                    if long == "help" || arg.is_hide_set() {
                        continue;
                    }
                    assert!(text.contains(&format!("--{long}")), "{name}: --{long}");
                }
            }
        }
    }
    for route in [
        "inbox",
        "mentions",
        "calls",
        "meetings",
        "saved",
        "pinned",
        "person",
        "around",
        "countOnly",
        "fetch",
        "send",
    ] {
        assert!(text.contains(route), "{route}");
    }
    assert!(text.contains("[RESTRICTED]"));
    assert!(text.len() < 10000, "stays a one-page read");
}

#[test]
fn messages_around_without_chat() {
    let (_td, h) = home();
    let o = teams(&h, &["messages", "--around", MID, "--context", "1"]);
    assert_eq!(o.code, 0, "{}", o.err);
    assert!(o.out.contains("Launch crew"));
    assert!(o.out.contains("# earlier messages exist") && o.out.contains("# later messages exist"));
    assert_eq!(o.out.lines().filter(|l| l.starts_with(">>> ")).count(), 1);
    assert!(o.out.contains("group note 3"));
    let r = teams_json(
        &h,
        &["messages", "--around", &format!("{GROUP}/{MID}"), "--context", "1"],
    );
    assert_eq!(r["aroundMessageId"], MID);
    assert_eq!(r["messages"].as_array().unwrap().len(), 3);
}

#[test]
fn messages_needs_chat_or_around() {
    let (_td, h) = home();
    let o = teams(&h, &["messages"]);
    assert_eq!(o.code, 2);
    assert!(o.err.contains("chat is required"), "{}", o.err);
}

#[test]
fn chats_member() {
    let (_td, h) = home();
    let r = teams_json(&h, &["chats", "--member", "bob@corp.test"]);
    let ids: Vec<&str> = r["chats"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![DM_BOB]);
    assert_eq!(r["memberMatches"][0]["id"], BOB);
    let o = teams(&h, &["chats", "--member", "o"]);
    assert_eq!(o.code, 0);
    assert!(o.err.contains("matched") && o.err.contains("people"), "{}", o.err);
}

#[test]
fn search_count_and_history() {
    let (_td, h) = home();
    let o = teams(&h, &["search", "group", "--count"]);
    assert_eq!((o.code, o.out.trim()), (0, "6"));
    let r = teams_json(&h, &["search", "group", "--count"]);
    assert_eq!((r["countOnly"].as_bool(), r["matched"].as_i64()), (Some(true), Some(6)));

    let excl = teams_json(&h, &["search", "group -2", "--count"]);
    assert_eq!(excl["matched"].as_i64(), Some(5));
    let lead_excl = teams(&h, &["search", "--json", "--count", "--", "-2", "group"]);
    assert_eq!(lead_excl.code, 0, "{}", lead_excl.err);
    assert_eq!(
        serde_json::from_str::<Value>(&lead_excl.out).unwrap()["matched"].as_i64(),
        Some(5)
    );

    assert!(teams(&h, &["search", "budget"]).out.contains("0 of 0"));
    let o = teams(&h, &["search", "budget", "--include-history"]);
    assert_eq!(o.code, 0);
    assert!(o.out.contains("(matched earlier version)") && o.out.contains("earlier version"));
    assert!(o.out.contains("budget draft v1") && o.out.contains("final numbers"));
}

#[test]
fn person() {
    let (_td, h) = home();
    let o = teams(&h, &["people", "Ada", "Lovelace"]);
    assert_eq!(o.code, 0);
    for s in ["Ada Lovelace", "ada@corp.test", "Launch crew"] {
        assert!(o.out.contains(s), "{s}");
    }
    let r = teams_json(&h, &["people", "ada@corp.test"]);
    assert_eq!(r["person"]["id"], ADA);
    assert_eq!(r["person"]["messagesInArchive"], 8);
    let o = teams(&h, &["people", "nobody-here"]);
    assert_eq!(o.code, 2);
    assert!(o.err.contains("No person"), "{}", o.err);
}

#[test]
fn inbox() {
    let (_td, h) = home();
    let o = teams(&h, &["inbox"]);
    assert_eq!(o.code, 0);
    for section in [
        "## Unread chats",
        "## Mentions",
        "## Awaiting your reply",
        "## Unread activity",
    ] {
        assert!(o.out.contains(section), "{section}");
    }
    assert!(o.out.contains("Me please review"));
    let r = teams_json(&h, &["inbox", "--since", "30d"]);
    let mut keys: Vec<&String> = r["counts"].as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(
        keys,
        vec!["awaitingReply", "mentions", "unreadActivities", "unreadChats"]
    );
}

#[test]
fn mentions_calls_meetings_saved_pinned() {
    let (_td, h) = home();
    let o = teams(&h, &["mentions", "--unread"]);
    assert_eq!(o.code, 0);
    assert!(o.out.contains("(unread)") && o.out.contains("1795000000009") && o.out.contains("Launch crew"));
    assert_eq!(teams_json(&h, &["mentions", "--chat", "Bob"])["matched"], 0);

    let o = teams(&h, &["calls"]);
    assert_eq!(o.code, 0);
    assert!(
        o.out.contains("Missed") && o.out.contains("Launch crew") && o.out.contains("2m05s"),
        "{}",
        o.out
    );
    let r = teams_json(&h, &["calls", "--missed", "--with", "bob"]);
    assert_eq!(r["calls"][0]["id"], "c2");
    assert_eq!(r["calls"].as_array().unwrap().len(), 1);

    let o = teams(&h, &["meetings", "--grep", "sprint"]);
    assert_eq!(o.code, 0);
    for s in [
        "Sprint review",
        "organizer Ada Lovelace",
        "2 attendees",
        "https://teams.test/join/m1",
        MEETING_CHAT,
    ] {
        assert!(o.out.contains(s), "{s}: {}", o.out);
    }
    assert_eq!(teams_json(&h, &["meetings", "--grep", "nothing"])["matched"], 0);

    let o = teams(&h, &["saved", "--all"]);
    assert!(o.code == 0 && o.out.contains("group note 1") && o.out.contains("Launch crew"));
    let o = teams(&h, &["pinned", "--chat", "Launch"]);
    assert!(o.code == 0 && o.out.contains("Subject: Rota") && o.out.contains("group note 2"));
    assert_eq!(teams_json(&h, &["pinned"])["matched"], 1);
}

#[test]
fn status_changes_records_people_attachments_json() {
    let (_td, h) = home();
    let o = teams(&h, &["status", "--json"]);
    assert_eq!(o.code, 1, "{}", o.err);
    let st: Value = serde_json::from_str(&o.out).unwrap();
    assert_eq!(st["healthy"], false);
    assert_eq!(
        st["archive"]["schemaVersion"],
        teams_cli::storage::archive::SCHEMA_VERSION
    );
    assert_eq!(st["counts"]["messages"], 9);
    let ch = teams_json(&h, &["changes", "--since", "all"]);
    assert!(
        ch["changes"].as_array().unwrap().iter().any(|c| c["id"] == EDITED),
        "{ch}"
    );
    let src = teams_json(&h, &["debug", "records"]);
    assert!(src.to_string().contains(CALLS), "{src}");
    let p = teams_json(&h, &["people", "--grep", "ada"]);
    assert_eq!(p["people"][0]["id"], ADA);
    assert!(teams_json(&h, &["attachments"]).get("attachments").is_some());
}

#[test]
fn export_stdout_and_private_file() {
    let (td, h) = home();
    let o = teams(&h, &["export", "Launch crew", "--history"]);
    assert_eq!(o.code, 0);
    assert!(o.out.starts_with("# Launch crew") && o.out.contains("final numbers"));
    let dest = td.path().join("out.md");
    let o = teams(&h, &["export", "Launch crew", "--out", dest.to_str().unwrap()]);
    assert_eq!((o.code, o.out.as_str()), (0, ""));
    assert!(std::fs::read_to_string(&dest).unwrap().starts_with("# Launch crew"));
    assert_eq!(std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777, 0o600);
    let tmp: Vec<_> = std::fs::read_dir(td.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(tmp.is_empty());
}

#[test]
fn verify() {
    let (td, h) = home();
    let o = teams(&h, &["archive", "verify", "--no-backups"]);
    assert_eq!(o.code, 0, "{}", o.err);
    assert!(o.out.starts_with("OK") && o.out.contains("integrity"), "{}", o.out);
    let r = teams_json(&h, &["archive", "verify"]);
    assert_eq!(r["ok"], true);
    assert_eq!(r["errors"], serde_json::json!([]));
    let bad = td.path().join("bad.sqlite3");
    std::fs::write(&bad, b"not a database".repeat(100)).unwrap();
    let o = teams(&h, &["archive", "verify", bad.to_str().unwrap()]);
    assert_eq!(o.code, 1);
    assert!(
        o.out.starts_with("FAILED") && !o.err.is_empty(),
        "{} / {}",
        o.out,
        o.err
    );
}

#[test]
fn backup_then_restore_dry_run_and_damaged_backup() {
    let (td, h) = home();
    let dest = td.path().join("snap.sqlite3.xz");
    let o = teams(&h, &["archive", "backup", "--out", dest.to_str().unwrap()]);
    assert_eq!(o.code, 0, "{}", o.err);
    assert_eq!(std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777, 0o600);

    let o = teams(&h, &["archive", "restore", dest.to_str().unwrap()]);
    assert_eq!(o.code, 0, "{}", o.err);
    assert!(o.out.contains("--yes") && o.out.contains("verified OK"), "{}", o.out);
    let r = teams_json(&h, &["archive", "restore", dest.to_str().unwrap()]);
    assert_eq!(
        r,
        serde_json::json!({"dryRun": true, "backup": dest.to_str().unwrap(), "verified": true,
                           "archive": h.join("archive.sqlite3").to_str().unwrap()})
    );

    let bad = td.path().join("bad.sqlite3");
    std::fs::write(&bad, b"garbage".repeat(200)).unwrap();
    let o = teams(&h, &["archive", "restore", bad.to_str().unwrap(), "--yes"]);
    assert_eq!(o.code, 1);
    assert!(o.err.contains("failed verification"), "{}", o.err);
}

#[test]
fn rejects_bad_numbers_with_exit_2() {
    let td = tempfile::tempdir().unwrap();
    for argv in [
        &["chats", "--limit", "-1"][..],
        &["messages", "x", "--context", "-2"],
        &["search", "x", "--limit", "nope"],
    ] {
        let o = teams(td.path(), argv);
        assert_eq!(o.code, 2, "{argv:?}");
        assert!(
            o.err.contains("error:") && (o.err.contains("0 or more") || o.err.contains("whole number")),
            "{argv:?}: {}",
            o.err
        );
    }
}

#[test]
fn unknown_command_is_usage_error() {
    let td = tempfile::tempdir().unwrap();
    assert_eq!(teams(td.path(), &["definitely-not-a-command"]).code, 2);
}

#[test]
fn no_archive_yet_is_a_clean_error() {
    let td = tempfile::tempdir().unwrap();
    let o = teams(td.path(), &["chats"]);
    assert_ne!(o.code, 0);
    assert!(o.err.starts_with("teams: "), "{}", o.err);
    assert!(!o.err.contains("panicked"), "{}", o.err);
}

#[test]
fn fetch_needs_a_target() {
    let (_td, h) = home();
    let o = teams(&h, &["fetch"]);
    assert_eq!(o.code, 2);
    assert!(o.err.contains("Provide a URL"), "{}", o.err);
}

#[test]
fn send_is_restricted_and_dry_run_by_default() {
    let (_td, h) = home();
    let o = teams(&h, &["send", "48:notes"]);
    assert_eq!(o.code, 2);
    assert!(o.err.contains("Provide message text"), "{}", o.err);

    let o = teams(&h, &["send", "48:notes", "hello", "world"]);
    assert_eq!(o.code, 0, "{}", o.err);
    assert!(
        o.out.contains("RESTRICTED") && o.out.contains("Nothing was sent"),
        "{}",
        o.out
    );
    let r = teams_json(&h, &["send", "48:notes", "hello"]);
    assert_eq!(
        (r["dryRun"].as_bool(), r["restricted"].as_bool(), r["sent"].as_bool()),
        (Some(true), Some(true), Some(false))
    );

    let o = teams(&h, &["send", "48:notes", "hello", "--yes"]);
    assert_eq!(o.code, 1);
    assert!(o.err.contains("TEAMS_CLI_ENABLE_SEND"), "{}", o.err);

    let o = teams_env(
        &h,
        &["send", GROUP, "hello", "--yes"],
        &[("TEAMS_CLI_ENABLE_SEND", "1"), ("TEAMS_CLI_SEND_ALLOWLIST", "48:notes")],
    );
    assert_eq!(o.code, 1, "{}", o.out);
    assert!(
        o.err.contains("TEAMS_CLI_SEND_ALLOWLIST") || o.err.contains("allowlist"),
        "{}",
        o.err
    );
}

#[test]
fn completions_migrations_doctor_and_interval_validation() {
    let (_td, h) = home();
    for shell in ["zsh", "bash", "fish"] {
        let o = teams(&h, &["completions", shell]);
        assert_eq!(o.code, 0, "{shell}: {}", o.err);
        assert!(!o.out.trim().is_empty(), "{shell}");
    }
    let m = teams_json(&h, &["archive", "migrations"]);
    assert_eq!(m["currentVersion"], teams_cli::storage::archive::SCHEMA_VERSION);
    assert_eq!(m["pending"].as_array().unwrap().len(), 0);
    assert_eq!(
        m["applied"].as_array().unwrap().len(),
        teams_cli::storage::archive::SCHEMA_VERSION as usize
    );
    let doc = teams(&h, &["doctor", "--json"]);
    let dv: Value = serde_json::from_str(&doc.out).unwrap();
    assert!(dv["checks"].is_array());
    assert!(!dv["checks"].as_array().unwrap().is_empty());

    let bad = teams(&h, &["daemon", "run", "--interval", "5"]);
    assert_eq!(bad.code, 2, "{}", bad.err);
    assert!(bad.err.contains("at least 10"), "{}", bad.err);
}

#[test]
fn dynamic_completions_work_across_all_shells() {
    let (_td, h) = home();
    for (shell, marker) in [
        ("fish", "teams __complete --shell fish"),
        ("zsh", "teams __complete --shell zsh"),
        ("bash", "teams __complete --shell bash"),
        ("elvish", "teams __complete --shell elvish"),
        ("powershell", "teams __complete --shell powershell"),
    ] {
        let o = teams(&h, &["completions", shell]);
        assert_eq!(o.code, 0, "{shell}: {}", o.err);
        assert!(o.out.contains(marker), "{shell}: {}", o.out);
    }

    let root = teams(&h, &["__complete", "--shell", "fish", "--", ""]);
    assert_eq!(root.code, 0);
    assert!(root.out.contains("inbox\t"));
    assert!(root.out.contains("daemon\t"));
    assert!(!root.out.contains("__complete"));
    assert!(root.out.trim_end().ends_with(":nofiles"));

    let daemon_sub = teams(&h, &["__complete", "--shell", "zsh", "--", "daemon", ""]);
    assert!(daemon_sub.out.contains("install\t") && daemon_sub.out.contains("logs\t"));

    let flags = teams(&h, &["__complete", "--shell", "fish", "--", "update", "--"]);
    assert!(flags.out.contains("--check\t"));
    assert!(!flags.out.contains("--background-check"));

    let chats = teams(&h, &["__complete", "--shell", "fish", "--", "messages", ""]);
    assert!(chats.out.contains("Launch crew\tchat,"), "{}", chats.out);
    assert!(chats.out.trim_end().ends_with(":nofiles"));

    let chats_bash = teams(&h, &["__complete", "--shell", "bash", "--", "search", "--chat=Lau"]);
    assert!(chats_bash.out.contains("--chat=Launch crew\n"), "{}", chats_bash.out);
    assert!(!chats_bash.out.contains('\t'), "bash omits descriptions");

    let people = teams(&h, &["__complete", "--shell", "zsh", "--", "people", ""]);
    assert!(people.out.contains("Ada Lovelace\tada@corp.test"), "{}", people.out);

    let sources = teams(&h, &["__complete", "--shell", "fish", "--", "debug", "records", ""]);
    assert!(
        sources.out.contains("call-history-manager/call-history\t"),
        "{}",
        sources.out
    );

    let times = teams(&h, &["__complete", "--shell", "fish", "--", "inbox", "--since", ""]);
    assert!(times.out.contains("today\t") && times.out.contains("7d\t"));

    let expires = teams(
        &h,
        &[
            "__complete",
            "--shell",
            "fish",
            "--",
            "token",
            "create",
            "ci",
            "--expires",
            "",
        ],
    );
    assert!(expires.out.contains("30d\t") && expires.out.contains("never\t"));

    let kinds = teams(&h, &["__complete", "--shell", "fish", "--", "chats", "--kind", ""]);
    assert!(kinds.out.contains("channel") && kinds.out.contains("meeting"));

    let out_flag = teams(
        &h,
        &["__complete", "--shell", "fish", "--", "export", "Launch", "--out", ""],
    );
    assert!(out_flag.out.trim_end().ends_with(":files"));

    let src_flag = teams(&h, &["__complete", "--shell", "fish", "--", "sync", "--source", ""]);
    assert!(src_flag.out.trim_end().ends_with(":dirs"));

    let restore = teams(&h, &["__complete", "--shell", "fish", "--", "archive", "restore", ""]);
    assert!(restore.out.trim_end().ends_with(":files"));

    let status_pos = teams(&h, &["__complete", "--shell", "fish", "--", "status", ""]);
    assert_eq!(status_pos.out.trim(), ":nofiles");
}
