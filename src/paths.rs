use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub const APP: &str = "teams-cli";
pub const ORIGIN: &str = "https_teams.microsoft.com_0.indexeddb";
pub const DEFAULT_PORT: u16 = 47613;
pub const DEFAULT_SYNC_INTERVAL: u64 = 60;
const CHUNK_SIZE: usize = 65_536;

pub fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

pub fn private_dir(p: PathBuf) -> io::Result<PathBuf> {
    fs::create_dir_all(&p)?;
    if let Ok(md) = fs::symlink_metadata(&p) {
        if !md.file_type().is_symlink() && (md.permissions().mode() & 0o777 != 0o700) {
            fs::set_permissions(&p, fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(p)
}

fn ensure_private_dir_once(p: PathBuf) -> PathBuf {
    use std::collections::HashSet;
    use std::sync::Mutex;

    static ENSURED: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);
    if let Ok(guard) = ENSURED.lock() {
        if guard.as_ref().is_some_and(|set| set.contains(&p)) && p.is_dir() {
            return p;
        }
    }
    if private_dir(p.clone()).is_ok() {
        if let Ok(mut guard) = ENSURED.lock() {
            guard.get_or_insert_with(HashSet::new).insert(p.clone());
        }
    }
    p
}

pub fn source_dir() -> PathBuf {
    if let Some(v) = env::var_os("TEAMS_CLI_SOURCE") {
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    home_dir()
        .join("Library/Containers/com.microsoft.teams2/Data/Library/Application Support")
        .join("Microsoft/MSTeams/EBWebView/WV2Profile_tfw/IndexedDB")
}

pub fn home() -> PathBuf {
    let p = match env::var_os("TEAMS_CLI_HOME") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home_dir().join("Library/Application Support").join(APP),
    };
    ensure_private_dir_once(p)
}

pub fn archive_path() -> PathBuf {
    home().join("archive.sqlite3")
}

pub fn api_token_path() -> PathBuf {
    home().join("api.token")
}

pub fn api_tokens_path() -> PathBuf {
    home().join("api.tokens.json")
}

pub fn lock_path() -> PathBuf {
    home().join("sync.lock")
}

pub fn update_check_path() -> PathBuf {
    home().join("update-check.json")
}

pub fn backups_dir() -> PathBuf {
    ensure_private_dir_once(home().join("backups"))
}

pub fn log_dir() -> PathBuf {
    let p = match env::var_os("TEAMS_CLI_HOME") {
        Some(v) if !v.is_empty() => home().join("logs"),
        _ => home_dir().join("Library/Logs").join(APP),
    };
    ensure_private_dir_once(p)
}

pub fn port() -> u16 {
    env::var("TEAMS_CLI_PORT")
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(DEFAULT_PORT)
}

pub fn expanduser(s: &str) -> PathBuf {
    if s == "~" {
        return home_dir();
    }
    if let Some(rest) = s.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    PathBuf::from(s)
}

pub fn private_tmpdir(near: &Path, prefix: &str) -> io::Result<tempfile::TempDir> {
    let parent = near.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let td = tempfile::Builder::new().prefix(prefix).tempdir_in(parent)?;
    fs::set_permissions(td.path(), fs::Permissions::from_mode(0o700))?;
    Ok(td)
}

pub fn fsync_file(path: &Path) -> io::Result<()> {
    let f = File::open(path)?;
    f.sync_all()
}

pub fn fsync_dir(path: &Path) {
    if let Ok(f) = File::open(path) {
        let _ = f.sync_all();
    }
}

fn random_suffix() -> io::Result<[u8; 6]> {
    let mut rnd = [0u8; 6];
    getrandom::getrandom(&mut rnd).map_err(|e| io::Error::other(format!("OS random number generator failed: {e}")))?;
    Ok(rnd)
}

pub fn atomic_write_bytes(dest: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let name = dest.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let rnd = random_suffix()?;
    let tmp = parent.join(format!(".{name}.{}.{}.tmp", std::process::id(), hex::encode(rnd)));
    let res = (|| -> io::Result<()> {
        let mut f = OpenOptions::new().write(true).create_new(true).mode(mode).open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
        fs::rename(&tmp, dest)?;
        fsync_dir(parent);
        Ok(())
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res
}

pub fn atomic_write_text(dest: &Path, text: &str) -> io::Result<()> {
    atomic_write_bytes(dest, text.as_bytes(), 0o600)
}

pub fn atomic_copy_file(src: &Path, dest: &Path, mode: u32, prefix: &str) -> io::Result<u64> {
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let rnd = random_suffix()?;
    let tmp = parent.join(format!("{prefix}{}.{}.partial", std::process::id(), hex::encode(rnd)));
    let res = (|| -> io::Result<u64> {
        let mut fin = File::open(src)?;
        let mut fout = OpenOptions::new().write(true).create_new(true).mode(mode).open(&tmp)?;
        let mut buf = vec![0u8; CHUNK_SIZE];
        let mut total = 0u64;
        loop {
            let n = fin.read(&mut buf)?;
            if n == 0 {
                break;
            }
            fout.write_all(&buf[..n])?;
            total += n as u64;
        }
        fout.sync_all()?;
        drop(fout);
        fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
        fs::rename(&tmp, dest)?;
        fsync_dir(parent);
        Ok(total)
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res
}

pub fn disk_free_bytes(path: &Path) -> u64 {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    static CACHE: Mutex<Option<(PathBuf, Instant, u64)>> = Mutex::new(None);
    const TTL: Duration = Duration::from_secs(10);

    let target = path.ancestors().find(|p| p.exists()).unwrap_or_else(|| Path::new("/"));
    if let Ok(guard) = CACHE.lock() {
        if let Some((ref cached_target, at, bytes)) = *guard {
            if cached_target == target && at.elapsed() < TTL {
                return bytes;
            }
        }
    }
    let bytes = disk_free_bytes_uncached(target);
    if bytes > 0 {
        if let Ok(mut guard) = CACHE.lock() {
            *guard = Some((target.to_path_buf(), Instant::now(), bytes));
        }
    }
    bytes
}

fn disk_free_bytes_uncached(target: &Path) -> u64 {
    let Ok(out) = std::process::Command::new("df").arg("-Pk").arg(target).output() else {
        return 0;
    };
    if !out.status.success() {
        return 0;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let Some(line) = text.lines().nth(1) else {
        return 0;
    };
    parse_df_pk_available_bytes(line)
}

pub fn parse_df_pk_available_bytes(line: &str) -> u64 {
    let cols: Vec<&str> = line.split_whitespace().collect();
    if let Some(cap_idx) = cols
        .iter()
        .position(|c| c.len() >= 2 && c.ends_with('%') && c[..c.len() - 1].bytes().all(|b| b.is_ascii_digit()))
    {
        if cap_idx >= 3
            && cols[cap_idx - 3].bytes().all(|b| b.is_ascii_digit())
            && cols[cap_idx - 2].bytes().all(|b| b.is_ascii_digit())
            && cols[cap_idx - 1].bytes().all(|b| b.is_ascii_digit())
        {
            if let Ok(kib) = cols[cap_idx - 1].parse::<u64>() {
                return kib.saturating_mul(1024);
            }
        }
    }
    cols.get(3)
        .and_then(|s| s.parse::<u64>().ok())
        .map(|kib| kib.saturating_mul(1024))
        .unwrap_or(0)
}

pub fn run_command_with_timeout(
    cmd: &mut std::process::Command,
    timeout: std::time::Duration,
) -> Result<std::process::Output, CommandTimeoutError> {
    use std::process::Stdio;
    use std::thread;
    use std::time::Instant;

    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(CommandTimeoutError::Io)?;
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    let out_reader = thread::spawn(move || -> Vec<u8> {
        let mut buf = Vec::new();
        if let Some(mut s) = stdout_pipe {
            let _ = s.read_to_end(&mut buf);
        }
        buf
    });
    let err_reader = thread::spawn(move || -> Vec<u8> {
        let mut buf = Vec::new();
        if let Some(mut s) = stderr_pipe {
            let _ = s.read_to_end(&mut buf);
        }
        buf
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(CommandTimeoutError::TimedOut);
                }
                thread::sleep(std::time::Duration::from_millis(25));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = out_reader.join();
                let _ = err_reader.join();
                return Err(CommandTimeoutError::Io(e));
            }
        }
    };
    let stdout = out_reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();
    Ok(std::process::Output { status, stdout, stderr })
}

#[derive(Debug)]
pub enum CommandTimeoutError {
    TimedOut,
    Io(io::Error),
}
