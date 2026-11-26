use crate::daemon;
use crate::launchd;
use crate::paths;
use crate::query::api;
use crate::storage::{archive, backups, migrations};
use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct CheckResult {
    pub name: &'static str,
    pub ok: bool,
    pub summary: String,
    pub hint: Option<String>,
}

impl CheckResult {
    fn pass(name: &'static str, summary: impl Into<String>) -> Self {
        Self {
            name,
            ok: true,
            summary: summary.into(),
            hint: None,
        }
    }

    fn fail(name: &'static str, summary: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            name,
            ok: false,
            summary: summary.into(),
            hint: Some(hint.into()),
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "ok": self.ok,
            "summary": self.summary,
            "hint": self.hint,
        })
    }
}

fn probe_http(port: u16, path: &str, token: Option<&str>) -> Result<(u16, Option<String>), String> {
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(2))
        .map_err(|e| format!("cannot connect to 127.0.0.1:{port}: {e}"))?;
    let _ = s.set_read_timeout(Some(Duration::from_secs(3)));
    let _ = s.set_write_timeout(Some(Duration::from_secs(2)));
    let mut req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n");
    if let Some(t) = token {
        req.push_str(&format!("Authorization: Bearer {t}\r\n"));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    let mut buf = [0u8; 1024];
    let n = s.read(&mut buf).map_err(|e| e.to_string())?;
    let text = std::str::from_utf8(&buf[..n]).unwrap_or("");
    let mut lines = text.lines();
    let status_line = lines.next().unwrap_or("");
    let code = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| format!("unexpected HTTP response: {status_line:?}"))?;
    let server = lines.take_while(|l| !l.is_empty()).find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case("Server").then(|| v.trim().to_string())
    });
    Ok((code, server))
}

fn listening_pids(port: u16) -> Vec<String> {
    let Ok(out) = std::process::Command::new("lsof")
        .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-Fp"])
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.strip_prefix('p').map(str::trim).filter(|s| !s.is_empty()))
        .map(String::from)
        .collect()
}

pub fn run_diagnostics() -> Value {
    let mut checks = Vec::new();

    let src = paths::source_dir();
    let ldb = src.join(format!("{}.leveldb", paths::ORIGIN));
    checks.push(if !ldb.is_dir() {
        CheckResult::fail(
            "teams_cache",
            format!("not found at {}", ldb.display()),
            "Ensure Microsoft Teams (v2) is installed and signed in, or grant terminal/agent App Data access in macOS Privacy & Security.",
        )
    } else {
        let ldb_clone = ldb.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let _ = std::thread::spawn(move || {
            let res = fs::read_dir(&ldb_clone)
                .map(|rd| rd.count())
                .map_err(|e| e.to_string());
            let _ = tx.send(res);
        });
        match rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(count)) => CheckResult::pass(
                "teams_cache",
                format!("readable ({count} entries in {})", ldb.display()),
            ),
            Ok(Err(e)) => CheckResult::fail(
                "teams_cache",
                format!("cannot read {}: {e}", ldb.display()),
                "Approve macOS 'access data from other apps' permission for your terminal and LaunchAgent.",
            ),
            Err(_) => CheckResult::fail(
                "teams_cache",
                format!("timed out reading {} (waiting on macOS privacy prompt)", ldb.display()),
                "Approve the macOS 'access data from other apps' prompt or grant access in System Settings -> Privacy & Security.",
            ),
        }
    });

    let ap = paths::archive_path();
    let mut opened_conn = None;
    checks.push(if !ap.is_file() {
        CheckResult::fail(
            "archive",
            format!("missing at {}", ap.display()),
            "Run `teams sync` to create and populate the archive.",
        )
    } else {
        match archive::connect_readonly(Some(&ap)) {
            Ok(conn) => {
                let ver: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap_or(0);
                let pending = migrations::pending(&conn).map(|p| p.len()).unwrap_or(0);
                let hist_len = migrations::history(&conn).map(|h| h.len()).unwrap_or(0);
                let msgs: i64 = conn
                    .query_row("SELECT COUNT(*) FROM messages WHERE kind = 'message'", [], |r| r.get(0))
                    .unwrap_or(0);
                let check = if ver < archive::SCHEMA_VERSION {
                    CheckResult::fail(
                        "archive",
                        format!(
                            "schema v{ver} ({pending} pending migration(s); target v{})",
                            archive::SCHEMA_VERSION
                        ),
                        "Run `teams sync` or `teams status` to upgrade the schema cleanly.",
                    )
                } else {
                    CheckResult::pass(
                        "archive",
                        format!("schema v{ver} ({hist_len} migrations recorded, {msgs} messages)"),
                    )
                };
                opened_conn = Some(conn);
                check
            }
            Err(e) => CheckResult::fail(
                "archive",
                format!("failed opening {}: {e}", ap.display()),
                "Run `teams archive verify` to inspect archive integrity.",
            ),
        }
    });

    let free = paths::disk_free_bytes(&ap);
    checks.push(if free == 0 || free >= backups::MIN_FREE_BYTES {
        CheckResult::pass("disk_space", format!("{:.1} GB free", free as f64 / 1e9))
    } else {
        CheckResult::fail(
            "disk_space",
            format!(
                "only {:.1} GB free (minimum {:.1} GB)",
                free as f64 / 1e9,
                backups::MIN_FREE_BYTES as f64 / 1e9
            ),
            "Free disk space so daily backups and syncs can proceed safely.",
        )
    });

    if let Some(ref conn) = opened_conn {
        let last_ok = archive::last_run(conn, true);
        let last_any = archive::last_run(conn, false);
        checks.push(match (last_ok, last_any) {
            (None, _) => CheckResult::fail(
                "sync",
                "no successful sync recorded yet",
                "Run `teams sync` to import messages.",
            ),
            (_, Some(ref l)) if l.status == "error" => CheckResult::fail(
                "sync",
                format!("last sync failed: {}", l.message.as_deref().unwrap_or("unknown error")),
                "Run `teams sync` to inspect the error or check `teams daemon logs`.",
            ),
            (Some(ok), _) => {
                let age_mins = ok
                    .finished_at
                    .map(|fin| (archive::now_ms() - fin).max(0) / 60_000)
                    .unwrap_or(0);
                CheckResult::pass("sync", format!("last successful sync {age_mins}m ago"))
            }
        });

        let bks = backups::list_backups(None);
        let scheduled_bks: Vec<&std::path::PathBuf> = bks
            .iter()
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(backups::is_scheduled_backup)
            })
            .collect();
        let bk_err = archive::get_meta(conn, "backup_error").filter(|s| !s.is_empty());
        checks.push(if let Some(err) = bk_err {
            CheckResult::fail(
                "backups",
                format!("last backup failed: {err}"),
                "Run `teams archive backup` to retry.",
            )
        } else if let Some(latest) = scheduled_bks.last() {
            let age_h = latest
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok()?.duration_since(UNIX_EPOCH).ok())
                .map(|d| (archive::now_ms() / 1000 - d.as_secs() as i64).max(0) / 3600)
                .unwrap_or(0);
            if age_h >= 48 {
                CheckResult::fail(
                    "backups",
                    format!(
                        "{} scheduled backup(s) ({} total), latest is {age_h}h old",
                        scheduled_bks.len(),
                        bks.len()
                    ),
                    "Run `teams archive backup` to create a fresh verified backup.",
                )
            } else {
                CheckResult::pass(
                    "backups",
                    format!(
                        "{} scheduled backup(s) ({} total), latest {age_h}h ago",
                        scheduled_bks.len(),
                        bks.len()
                    ),
                )
            }
        } else {
            CheckResult::fail(
                "backups",
                if bks.is_empty() {
                    "no backups found".to_string()
                } else {
                    format!("no scheduled backups ({} safety backup(s) only)", bks.len())
                },
                "Run `teams archive backup` to create an initial verified backup.",
            )
        });
    }

    let agent = launchd::status();
    let loaded = agent["loaded"].as_bool().unwrap_or(false);
    let exe_match = agent["executableMatches"].as_bool();
    checks.push(if !loaded {
        CheckResult::fail(
            "daemon_agent",
            "launchd agent is not loaded",
            "Run `teams daemon install` to start the background sync and local API.",
        )
    } else if exe_match == Some(false) {
        CheckResult::fail(
            "daemon_agent",
            format!(
                "running (pid {}) but plist points to {} instead of current binary",
                agent["pid"].as_str().unwrap_or("?"),
                agent["plistExecutable"].as_str().unwrap_or("?")
            ),
            "Run `teams daemon install` to update the LaunchAgent to the current binary.",
        )
    } else {
        CheckResult::pass(
            "daemon_agent",
            format!("running (pid {}, binary matches)", agent["pid"].as_str().unwrap_or("?")),
        )
    });

    let port = paths::port();
    let daemon_pid = agent["pid"].as_str().map(String::from);
    let api_check = match probe_http(port, "/health", None) {
        Ok((200, server)) => {
            let server_ok = server.as_deref().is_some_and(|s| s.starts_with("teams-cli/"));
            let pids = listening_pids(port);
            let owner_ok = daemon_pid.as_deref().is_some_and(|dp| pids.iter().any(|p| p == dp));
            if !server_ok || !owner_ok {
                CheckResult::fail(
                    "local_api",
                    format!(
                        "127.0.0.1:{port} is not verified as the teams-cli LaunchAgent (refusing to send bearer token)"
                    ),
                    "Run `teams daemon install` or check what process owns the port (`lsof -iTCP:47613 -sTCP:LISTEN`).",
                )
            } else {
                match probe_http(port, "/v1/status", None) {
                    Ok((401, _)) => match daemon::list_named_tokens(false) {
                        Ok(v) => {
                            let named_n = v["count"].as_u64().unwrap_or(0);
                            let note = if named_n == 0 {
                                "0 active tokens; run `teams token create <name>` to grant API access".to_string()
                            } else {
                                format!("{named_n} active named token(s)")
                            };
                            CheckResult::pass(
                                "local_api",
                                format!("http://127.0.0.1:{port} healthy and bearer auth enforced ({note})"),
                            )
                        }
                        Err(e) => CheckResult::fail(
                            "local_api",
                            format!("API token store error: {e}"),
                            "Check permissions or JSON validity of api.tokens.json.",
                        ),
                    },
                    Ok((code, _)) => CheckResult::fail(
                        "local_api",
                        format!(
                            "http://127.0.0.1:{port}/v1/status returned HTTP {code} without a bearer token (expected 401)"
                        ),
                        "Run `teams daemon restart` and check `teams daemon logs`.",
                    ),
                    Err(e) => CheckResult::fail("local_api", e, "Check `teams daemon logs`."),
                }
            }
        }
        Ok((code, _)) => CheckResult::fail(
            "local_api",
            format!("http://127.0.0.1:{port}/health returned HTTP {code}"),
            "Check whether another service is using the port or set `TEAMS_CLI_PORT`.",
        ),
        Err(e) => CheckResult::fail(
            "local_api",
            e,
            "Run `teams daemon install` (or `teams daemon run` in a terminal).",
        ),
    };
    checks.push(api_check);

    if let Some(ref conn) = opened_conn {
        if api::require_data(conn).is_err() {
            checks.push(CheckResult::fail(
                "archive_data",
                "archive has no completed sync runs",
                "Run `teams sync` to populate it.",
            ));
        }
    }

    let cookie_db = crate::live::cookies::cookies_path();
    checks.push(if cookie_db.is_file() {
        CheckResult::pass(
            "live_cookies",
            format!("Teams cookie store present at {}", cookie_db.display()),
        )
    } else {
        CheckResult::pass(
            "live_cookies",
            format!(
                "not present at {} (archive reads still work offline)",
                cookie_db.display()
            ),
        )
    });

    let ok = checks.iter().all(|c| c.ok);
    json!({
        "ok": ok,
        "version": crate::VERSION,
        "schemaVersion": archive::SCHEMA_VERSION,
        "checks": checks.iter().map(CheckResult::to_json).collect::<Vec<_>>(),
    })
}
