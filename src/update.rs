use crate::launchd;
use crate::live::http;
use flate2::read::GzDecoder;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::Duration;
use url::Url;

pub const REPO: &str = "sasanktumpati/teams-cli";
pub const API_BASE: &str = "https://api.github.com";
pub const MAX_API_BYTES: u64 = 2 * 1024 * 1024;
pub const MAX_ARCHIVE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;

#[derive(Debug, Clone)]
pub struct UpdateOptions {
    pub check_only: bool,
    pub version: Option<String>,
    pub force: bool,
}

#[derive(Debug, Clone)]
pub struct UpdateOutcome {
    pub current_version: String,
    pub target_version: String,
    pub tag: String,
    pub target_triple: String,
    pub update_available: bool,
    pub updated: bool,
    pub binary_path: String,
    pub sha256: Option<String>,
    pub daemon_restarted: bool,
    pub daemon_restart_error: Option<String>,
}

impl UpdateOutcome {
    pub fn to_json(&self) -> Value {
        json!({
            "currentVersion": self.current_version,
            "targetVersion": self.target_version,
            "tag": self.tag,
            "target": self.target_triple,
            "updateAvailable": self.update_available,
            "updated": self.updated,
            "binaryPath": self.binary_path,
            "sha256": self.sha256,
            "daemonRestarted": self.daemon_restarted,
            "daemonRestartError": self.daemon_restart_error,
        })
    }
}

pub fn current_target() -> Result<&'static str, String> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Ok("aarch64-apple-darwin")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Ok("x86_64-apple-darwin")
    } else {
        Err("Self-update prebuilt binaries are only published for macOS (arm64 and x86_64).".to_string())
    }
}

pub fn normalize_tag(v: &str) -> Result<String, String> {
    let trimmed = v.trim();
    if trimmed.is_empty() {
        return Err("Version tag cannot be empty.".to_string());
    }
    if trimmed.contains('/') || trimmed.contains("..") || trimmed.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(format!("Invalid version tag '{trimmed}'."));
    }
    if trimmed.starts_with('v') {
        Ok(trimmed.to_string())
    } else {
        Ok(format!("v{trimmed}"))
    }
}

pub fn parse_semver(v: &str) -> Option<(u64, u64, u64)> {
    let s = v.trim().strip_prefix('v').unwrap_or(v.trim());
    let core = s.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

pub fn is_newer(current: &str, candidate: &str) -> bool {
    match (parse_semver(current), parse_semver(candidate)) {
        (Some(cur), Some(cand)) => cand > cur,
        _ => current.trim().trim_start_matches('v') != candidate.trim().trim_start_matches('v'),
    }
}

pub fn is_trusted_release_url(raw: &str, allow_loopback_http: bool) -> bool {
    let Ok(url) = Url::parse(raw) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    if allow_loopback_http && url.scheme() == "http" && matches!(host, "127.0.0.1" | "localhost" | "[::1]") {
        return true;
    }
    if url.scheme() != "https" {
        return false;
    }
    matches!(
        host,
        "api.github.com"
            | "github.com"
            | "objects.githubusercontent.com"
            | "release-assets.githubusercontent.com"
            | "github-releases.githubusercontent.com"
    )
}

fn fetch_bytes(url: &str, accept: Option<&str>, max_bytes: u64, allow_loopback_http: bool) -> Result<Vec<u8>, String> {
    let agent = http::agent(Duration::from_secs(60));
    let mut current = url.to_string();
    for _ in 0..=MAX_REDIRECTS {
        if !is_trusted_release_url(&current, allow_loopback_http) {
            return Err(format!("Refusing untrusted download URL: {current}"));
        }
        let mut req = agent
            .get(&current)
            .set("User-Agent", &format!("teams-cli/{}", crate::VERSION));
        if let Some(a) = accept {
            req = req.set("Accept", a);
        }
        let resp = match req.call() {
            Ok(r) => r,
            Err(ureq::Error::Status(code, _)) => {
                return Err(format!("HTTP {code} fetching {current}"));
            }
            Err(ureq::Error::Transport(e)) => {
                return Err(format!("Network error fetching {current}: {e}"));
            }
        };
        let status = resp.status();
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            let loc = resp
                .header("location")
                .ok_or_else(|| format!("Redirect HTTP {status} without Location header from {current}"))?;
            let base = Url::parse(&current).map_err(|e| format!("Invalid URL {current}: {e}"))?;
            current = base
                .join(loc)
                .map_err(|e| format!("Invalid redirect Location '{loc}': {e}"))?
                .to_string();
            continue;
        }
        let mut out = Vec::new();
        let mut limited = resp.into_reader().take(max_bytes.saturating_add(1));
        limited
            .read_to_end(&mut out)
            .map_err(|e| format!("Failed reading response from {current}: {e}"))?;
        if (out.len() as u64) > max_bytes {
            return Err(format!("Download from {current} exceeded limit of {max_bytes} bytes."));
        }
        return Ok(out);
    }
    Err(format!("Too many redirects fetching {url}"))
}

pub fn parse_sha256_for_asset(sums_text: &str, asset_name: &str) -> Option<String> {
    for line in sums_text.lines() {
        let mut parts = line.split_whitespace();
        let Some(hash) = parts.next() else {
            continue;
        };
        let Some(name_raw) = parts.next() else {
            continue;
        };
        let name = name_raw.trim_start_matches('*');
        if name == asset_name && hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(hash.to_ascii_lowercase());
        }
    }
    None
}

pub fn verify_sha256(bytes: &[u8], expected_hex: &str) -> Result<String, String> {
    let actual = hex::encode(Sha256::digest(bytes));
    if actual != expected_hex.to_ascii_lowercase() {
        return Err(format!("SHA-256 mismatch: expected {expected_hex}, got {actual}"));
    }
    Ok(actual)
}

fn parse_octal(field: &[u8]) -> Result<u64, String> {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    let s = std::str::from_utf8(&field[..end])
        .map_err(|_| "Invalid UTF-8 in tar header".to_string())?
        .trim();
    if s.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(s, 8).map_err(|_| format!("Invalid octal size '{s}' in tar header"))
}

fn parse_tar_name(header: &[u8; 512]) -> String {
    let name_bytes = &header[0..100];
    let name_end = name_bytes.iter().position(|&b| b == 0).unwrap_or(name_bytes.len());
    let name = String::from_utf8_lossy(&name_bytes[..name_end]);
    let prefix_bytes = &header[345..500];
    let prefix_end = prefix_bytes.iter().position(|&b| b == 0).unwrap_or(prefix_bytes.len());
    let prefix = String::from_utf8_lossy(&prefix_bytes[..prefix_end]);
    if prefix.is_empty() {
        name.into_owned()
    } else {
        format!("{prefix}/{name}")
    }
}

pub fn extract_binary_from_tar_gz(tar_gz: &[u8], binary_name: &str) -> Result<Vec<u8>, String> {
    let mut decoder = GzDecoder::new(tar_gz);
    let mut tar_bytes = Vec::new();
    decoder
        .by_ref()
        .take(MAX_ARCHIVE_BYTES.saturating_add(1))
        .read_to_end(&mut tar_bytes)
        .map_err(|e| format!("Failed decompressing release archive: {e}"))?;
    if (tar_bytes.len() as u64) > MAX_ARCHIVE_BYTES {
        return Err("Decompressed release archive exceeded size limit.".to_string());
    }

    let mut pos = 0usize;
    let mut next_long_name: Option<String> = None;
    while pos + 512 <= tar_bytes.len() {
        let header: &[u8; 512] = tar_bytes[pos..pos + 512]
            .try_into()
            .map_err(|_| "Truncated tar header".to_string())?;
        pos += 512;
        if header.iter().all(|&b| b == 0) {
            break;
        }
        let size = parse_octal(&header[124..136])? as usize;
        let typeflag = header[156];
        let padded = size
            .checked_add(511)
            .ok_or_else(|| "Tar entry size overflow".to_string())?
            & !511;
        if pos.checked_add(padded).is_none_or(|end| end > tar_bytes.len()) {
            return Err("Truncated tar entry in release archive.".to_string());
        }
        let data = &tar_bytes[pos..pos + size];
        pos += padded;

        if typeflag == b'L' {
            let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
            next_long_name = Some(String::from_utf8_lossy(&data[..end]).into_owned());
            continue;
        }
        if typeflag == b'x' || typeflag == b'g' {
            continue;
        }

        let raw_name = next_long_name.take().unwrap_or_else(|| parse_tar_name(header));
        let clean = raw_name.trim_start_matches("./");
        if clean.contains("..") || clean.starts_with('/') {
            continue;
        }
        let is_regular = typeflag == 0 || typeflag == b'0';
        if is_regular && clean == binary_name {
            if data.is_empty() {
                return Err(format!("Extracted '{binary_name}' binary is empty."));
            }
            return Ok(data.to_vec());
        }
    }
    Err(format!("Release archive did not contain '{binary_name}' binary."))
}

pub fn is_valid_macho(bytes: &[u8]) -> bool {
    if bytes.len() < 4 {
        return false;
    }
    matches!(
        &bytes[0..4],
        [0xCF, 0xFA, 0xED, 0xFE]
            | [0xFE, 0xED, 0xFA, 0xCF]
            | [0xCE, 0xFA, 0xED, 0xFE]
            | [0xFE, 0xED, 0xFA, 0xCE]
            | [0xCA, 0xFE, 0xBA, 0xBE]
            | [0xBE, 0xBA, 0xFE, 0xCA]
    )
}

pub fn install_binary_atomically(dest: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = dest
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| format!("Destination '{}' has no parent directory.", dest.display()))?;
    let mut rand_bytes = [0u8; 6];
    getrandom::getrandom(&mut rand_bytes).map_err(|e| format!("Random suffix failed: {e}"))?;
    let tmp = parent.join(format!(
        ".teams-update-{}-{}.tmp",
        std::process::id(),
        hex::encode(rand_bytes)
    ));
    let write_res = (|| -> std::io::Result<()> {
        let mut f = OpenOptions::new().write(true).create_new(true).mode(0o755).open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        Ok(())
    })();
    if let Err(e) = write_res {
        let _ = fs::remove_file(&tmp);
        return Err(format!("Could not stage updated binary in {}: {e}", parent.display()));
    }
    if let Err(e) = fs::rename(&tmp, dest) {
        let _ = fs::remove_file(&tmp);
        return Err(format!("Could not replace {}: {e}", dest.display()));
    }
    Ok(())
}

pub fn run_update(opts: &UpdateOptions) -> Result<UpdateOutcome, String> {
    let target = current_target()?;
    let exe = env::current_exe().map_err(|e| format!("Could not locate current executable: {e}"))?;
    let exe = fs::canonicalize(&exe).unwrap_or(exe);
    let mut outcome = run_update_with(opts, API_BASE, REPO, crate::VERSION, target, &exe, false, true)?;
    if outcome.updated && launchd::status()["loaded"].as_bool().unwrap_or(false) {
        match launchd::restart() {
            Ok(_) => outcome.daemon_restarted = true,
            Err(e) => outcome.daemon_restart_error = Some(e),
        }
    }
    Ok(outcome)
}

#[allow(clippy::too_many_arguments)]
pub fn run_update_with(
    opts: &UpdateOptions,
    api_base: &str,
    repo: &str,
    current_version: &str,
    target_triple: &str,
    binary_path: &Path,
    allow_loopback_http: bool,
    require_macho: bool,
) -> Result<UpdateOutcome, String> {
    let release_url = match opts.version {
        Some(ref v) => {
            let tag = normalize_tag(v)?;
            format!("{}/repos/{repo}/releases/tags/{tag}", api_base.trim_end_matches('/'))
        }
        None => format!("{}/repos/{repo}/releases/latest", api_base.trim_end_matches('/')),
    };

    let release_bytes = fetch_bytes(
        &release_url,
        Some("application/vnd.github+json"),
        MAX_API_BYTES,
        allow_loopback_http,
    )?;
    let release: Value =
        serde_json::from_slice(&release_bytes).map_err(|e| format!("Invalid GitHub release JSON: {e}"))?;
    let tag = release["tag_name"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "GitHub release response is missing tag_name.".to_string())?
        .to_string();
    let target_version = tag.strip_prefix('v').unwrap_or(&tag).to_string();
    let update_available = is_newer(current_version, &target_version);
    let explicit_different =
        opts.version.is_some() && current_version.trim().trim_start_matches('v') != target_version.as_str();
    let should_install = opts.force || update_available || explicit_different;

    let mut outcome = UpdateOutcome {
        current_version: current_version.to_string(),
        target_version,
        tag: tag.clone(),
        target_triple: target_triple.to_string(),
        update_available,
        updated: false,
        binary_path: binary_path.to_string_lossy().into_owned(),
        sha256: None,
        daemon_restarted: false,
        daemon_restart_error: None,
    };

    if opts.check_only || !should_install {
        return Ok(outcome);
    }

    let asset_name = format!("teams-{tag}-{target_triple}.tar.gz");
    let assets = release["assets"]
        .as_array()
        .ok_or_else(|| format!("Release {tag} has no assets."))?;
    let find_asset_url = |name: &str| -> Result<String, String> {
        assets
            .iter()
            .find(|a| a["name"].as_str() == Some(name))
            .and_then(|a| a["browser_download_url"].as_str())
            .map(ToOwned::to_owned)
            .ok_or_else(|| format!("Release {tag} does not include asset '{name}'."))
    };

    let archive_url = find_asset_url(&asset_name)?;
    let sums_url = find_asset_url("SHA256SUMS.txt")?;

    let sums_bytes = fetch_bytes(&sums_url, None, MAX_API_BYTES, allow_loopback_http)?;
    let sums_text = String::from_utf8(sums_bytes).map_err(|_| "SHA256SUMS.txt is not valid UTF-8.".to_string())?;
    let expected_hash = parse_sha256_for_asset(&sums_text, &asset_name)
        .ok_or_else(|| format!("Checksum for '{asset_name}' not found in SHA256SUMS.txt."))?;

    let archive_bytes = fetch_bytes(&archive_url, None, MAX_ARCHIVE_BYTES, allow_loopback_http)?;
    let verified_hash = verify_sha256(&archive_bytes, &expected_hash)?;

    let binary_bytes = extract_binary_from_tar_gz(&archive_bytes, "teams")?;
    if require_macho && !is_valid_macho(&binary_bytes) {
        return Err("Extracted 'teams' binary is not a valid macOS Mach-O executable.".to_string());
    }

    install_binary_atomically(binary_path, &binary_bytes)?;
    outcome.updated = true;
    outcome.sha256 = Some(verified_hash);
    Ok(outcome)
}
