use crate::paths;
use regex::Regex;
use serde_json::{json, Value};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;
use std::thread;
use std::time::{Duration, Instant};

pub const LABEL: &str = "dev.teams-cli.daemon";

static STATE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n\s*state = (\S+)").unwrap());
static PID_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n\s*pid = (\d+)").unwrap());
static EXIT_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"last exit code = (.+)").unwrap());

pub fn plist_path() -> PathBuf {
    paths::home_dir()
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

fn domain() -> String {
    let uid = Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "501".to_string());
    format!("gui/{uid}")
}

fn executable() -> String {
    if let Ok(exe) = env::current_exe() {
        return exe.to_string_lossy().into_owned();
    }
    env::args().next().unwrap_or_else(|| "teams".to_string())
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

pub fn install(interval: u64, port_opt: Option<u16>, allow_writes: bool) -> Result<PathBuf, String> {
    let exe = executable();
    let exe_parent = PathBuf::from(&exe)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/usr/local/bin".to_string());
    let log_dir = paths::log_dir();
    let port = port_opt.unwrap_or_else(paths::port);
    let writes_arg = if allow_writes {
        "\n\t\t<string>--allow-writes</string>"
    } else {
        ""
    };
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>EnvironmentVariables</key>
	<dict>
		<key>PATH</key>
		<string>/usr/bin:/bin:/usr/sbin:/sbin:{path_dir}</string>
	</dict>
	<key>KeepAlive</key>
	<true/>
	<key>Label</key>
	<string>{label}</string>
	<key>LowPriorityIO</key>
	<true/>
	<key>ProgramArguments</key>
	<array>
		<string>{exe}</string>
		<string>daemon</string>
		<string>run</string>
		<string>--interval</string>
		<string>{interval}</string>
		<string>--port</string>
		<string>{port}</string>{writes_arg}
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>StandardErrorPath</key>
	<string>{err_log}</string>
	<key>StandardOutPath</key>
	<string>{out_log}</string>
	<key>ThrottleInterval</key>
	<integer>30</integer>
</dict>
</plist>
"#,
        path_dir = xml_escape(&exe_parent),
        label = LABEL,
        exe = xml_escape(&exe),
        interval = interval,
        port = port,
        writes_arg = writes_arg,
        err_log = xml_escape(&log_dir.join("launchd.err.log").to_string_lossy()),
        out_log = xml_escape(&log_dir.join("launchd.out.log").to_string_lossy()),
    );
    let path = plist_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    if !stop(10.0) {
        return Err(format!(
            "could not unload the previous {LABEL} agent (launchctl bootout did not complete within 10s); \
             run `launchctl bootout {}/{LABEL}` and retry",
            domain()
        ));
    }
    write_plist_atomic(&path, &plist).map_err(|e| format!("failed writing {}: {e}", path.display()))?;
    bootstrap(&path)
}

pub fn write_plist_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::Builder::new()
        .prefix(".teams-cli-plist-")
        .suffix(".tmp")
        .tempfile_in(parent)?;
    tmp.as_file_mut().write_all(contents.as_bytes())?;
    tmp.as_file().set_permissions(fs::Permissions::from_mode(0o644))?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    if let Ok(d) = fs::File::open(parent) {
        let _ = d.sync_all();
    }
    Ok(())
}

fn bootstrap(path: &Path) -> Result<PathBuf, String> {
    let mut last_err = String::new();
    for attempt in 0..10u64 {
        let out = Command::new("launchctl")
            .args(["bootstrap", &domain(), &path.to_string_lossy()])
            .output()
            .map_err(|e| e.to_string())?;
        if out.status.success() {
            return Ok(path.to_path_buf());
        }
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        last_err = if !stderr.is_empty() { stderr } else { stdout };
        thread::sleep(Duration::from_millis(500 * (attempt + 1)));
    }
    Err(format!("launchctl bootstrap failed: {last_err}"))
}

pub fn loaded() -> bool {
    Command::new("launchctl")
        .args(["print", &format!("{}/{LABEL}", domain())])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn start() -> Result<PathBuf, String> {
    let path = plist_path();
    if !path.exists() {
        return install(paths::DEFAULT_SYNC_INTERVAL, None, false);
    }
    if loaded() {
        return Ok(path);
    }
    bootstrap(&path)
}

pub fn stop(timeout_secs: f64) -> bool {
    let _ = Command::new("launchctl")
        .args(["bootout", &domain(), &plist_path().to_string_lossy()])
        .output();
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("{}/{LABEL}", domain())])
        .output();
    let deadline = Instant::now() + Duration::from_secs_f64(timeout_secs.max(0.0));
    while loaded() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(200));
    }
    !loaded()
}

pub fn uninstall() -> bool {
    let _ = stop(10.0);
    let path = plist_path();
    if path.exists() {
        let _ = fs::remove_file(&path);
        true
    } else {
        false
    }
}

pub fn plist_executable(plist_path: &Path) -> Option<String> {
    let text = fs::read_to_string(plist_path).ok()?;
    let after_key = text.split("<key>ProgramArguments</key>").nth(1)?;
    let start = after_key.find("<string>")? + "<string>".len();
    let end = after_key[start..].find("</string>")?;
    Some(
        after_key[start..start + end]
            .replace("&amp;", "&")
            .replace("&lt;", "<")
            .replace("&gt;", ">"),
    )
}

pub fn restart() -> Result<PathBuf, String> {
    let path = plist_path();
    if !path.exists() {
        return install(paths::DEFAULT_SYNC_INTERVAL, None, false);
    }
    if let Ok(text) = fs::read_to_string(&path) {
        let old = "<string>daemon</string>\n\t\t<string>--interval</string>";
        if text.contains(old) {
            let updated = text.replace(
                old,
                "<string>daemon</string>\n\t\t<string>run</string>\n\t\t<string>--interval</string>",
            );
            let _ = write_plist_atomic(&path, &updated);
        }
    }
    if !stop(10.0) {
        return Err(format!(
            "could not stop {LABEL}; run `launchctl bootout {}/{LABEL}` and retry",
            domain()
        ));
    }
    bootstrap(&path)
}

pub fn plist_allow_writes(plist_path: &Path) -> Option<bool> {
    let text = fs::read_to_string(plist_path).ok()?;
    let after_key = text.split("<key>ProgramArguments</key>").nth(1)?;
    let array = after_key.split("</array>").next()?;
    Some(array.contains("<string>--allow-writes</string>"))
}

pub fn status() -> Value {
    let path = plist_path();
    let installed = path.exists();
    let plist_exe = plist_executable(&path);
    let allow_writes = plist_allow_writes(&path);
    let current_exe = executable();
    let executable_matches = plist_exe.as_deref().map(|p| p == current_exe);
    let Ok(out) = Command::new("launchctl")
        .args(["print", &format!("{}/{LABEL}", domain())])
        .output()
    else {
        return json!({
            "installed": installed,
            "loaded": false,
            "allowWrites": allow_writes,
            "plistExecutable": plist_exe,
            "executableMatches": executable_matches,
        });
    };
    if !out.status.success() {
        return json!({
            "installed": installed,
            "loaded": false,
            "allowWrites": allow_writes,
            "plistExecutable": plist_exe,
            "executableMatches": executable_matches,
        });
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let pick = |re: &Regex| re.captures(&text).map(|c| c[1].trim().to_string());
    json!({
        "installed": installed,
        "loaded": true,
        "allowWrites": allow_writes,
        "state": pick(&STATE_RE),
        "pid": pick(&PID_RE),
        "lastExitCode": pick(&EXIT_RE),
        "plist": path.to_string_lossy(),
        "plistExecutable": plist_exe,
        "executableMatches": executable_matches,
    })
}
