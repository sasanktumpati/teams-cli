use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use base64::Engine;
use pbkdf2::pbkdf2_hmac;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::env;
use std::fmt;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use url::Url;

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;
type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;

const KEYCHAIN_SERVICE: &str = "Microsoft Teams Safe Storage";
const KEYCHAIN_ACCOUNT: &str = "Microsoft Teams";
const PBKDF2_SALT: &[u8] = b"saltysalt";
const PBKDF2_ITERS: u32 = 1003;
const AES_IV: [u8; 16] = [b' '; 16];
const WEBKIT_EPOCH_OFFSET_SEC: f64 = 11_644_473_600.0;
const COOKIE_COPY_ATTEMPTS: usize = 3;
const KEYCHAIN_TIMEOUT_SECS: u64 = 15;

pub const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
    AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

pub const TRUSTED_HOST_SUFFIXES: &[&str] = &[
    ".asm.skype.com",
    ".sharepoint.com",
    ".teams.microsoft.com",
    ".skype.com",
    ".office.com",
    ".microsoftonline.com",
];

static DERIVED_KEY_CACHE: Mutex<Option<[u8; 16]>> = Mutex::new(None);

#[derive(Debug, Clone)]
pub struct AuthError(pub String);

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for AuthError {}

#[derive(Clone)]
pub struct Cookie {
    pub host_key: String,
    pub name: String,
    pub value: String,
    pub path: String,
    pub expires_utc: i64,
    pub partitioned: bool,
}

impl fmt::Debug for Cookie {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cookie")
            .field("host_key", &self.host_key)
            .field("name", &self.name)
            .field("path", &self.path)
            .field("partitioned", &self.partitioned)
            .finish()
    }
}

pub fn cookies_path() -> PathBuf {
    if let Some(v) = env::var_os("TEAMS_CLI_COOKIES") {
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    crate::paths::home_dir()
        .join("Library/Containers/com.microsoft.teams2/Data/Library/Application Support")
        .join("Microsoft/MSTeams/EBWebView/WV2Profile_tfw/Cookies")
}

pub fn derive_aes_key(password: &str) -> [u8; 16] {
    let mut key = [0u8; 16];
    pbkdf2_hmac::<Sha1>(password.as_bytes(), PBKDF2_SALT, PBKDF2_ITERS, &mut key);
    key
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

pub fn encrypt_cookie_value(plaintext: &str, host_key: &str, password: &str, db_version: i64) -> Vec<u8> {
    let key = derive_aes_key(password);
    let mut raw = Vec::new();
    if db_version >= 24 {
        raw.extend_from_slice(&Sha256::digest(host_key.as_bytes()));
    }
    raw.extend_from_slice(plaintext.as_bytes());
    let ct = Aes128CbcEnc::new(&key.into(), &AES_IV.into()).encrypt_padded_vec_mut::<Pkcs7>(&raw);
    let mut out = Vec::with_capacity(3 + ct.len());
    out.extend_from_slice(b"v10");
    out.extend_from_slice(&ct);
    out
}

pub fn decrypt_cookie_value(
    encrypted_value: &[u8],
    host_key: &str,
    key: &[u8; 16],
    db_version: i64,
    plain_value: &str,
) -> Result<String, AuthError> {
    if encrypted_value.is_empty() {
        return Ok(plain_value.to_string());
    }
    if !encrypted_value.starts_with(b"v10") {
        if !plain_value.is_empty() {
            return Ok(plain_value.to_string());
        }
        return Err(AuthError("Unsupported cookie encryption version".to_string()));
    }
    let mut dec = Aes128CbcDec::new(key.into(), &AES_IV.into())
        .decrypt_padded_vec_mut::<Pkcs7>(&encrypted_value[3..])
        .map_err(|_| AuthError("AES-128-CBC decryption failed".to_string()))?;
    if db_version >= 24 {
        let expected = Sha256::digest(host_key.as_bytes());
        if dec.len() < 32 || !ct_eq(&dec[..32], &expected) {
            return Err(AuthError("Cookie host verification failed".to_string()));
        }
        dec.drain(..32);
    }
    String::from_utf8(dec).map_err(|e| AuthError(format!("Decrypted cookie is not valid UTF-8: {e}")))
}

pub fn get_keychain_aes_key() -> Result<[u8; 16], AuthError> {
    if let Ok(guard) = DERIVED_KEY_CACHE.lock() {
        if let Some(k) = *guard {
            return Ok(k);
        }
    }
    let mut cmd = Command::new("security");
    cmd.args([
        "find-generic-password",
        "-s",
        KEYCHAIN_SERVICE,
        "-a",
        KEYCHAIN_ACCOUNT,
        "-w",
    ]);
    let output = match crate::paths::run_command_with_timeout(&mut cmd, Duration::from_secs(KEYCHAIN_TIMEOUT_SECS)) {
        Ok(o) => o,
        Err(crate::paths::CommandTimeoutError::TimedOut) => {
            return Err(AuthError(format!(
                "macOS Keychain access timed out after {KEYCHAIN_TIMEOUT_SECS}s while reading \
                 'Microsoft Teams Safe Storage'. Unlock your Keychain or approve the Keychain \
                 access prompt, then try again."
            )));
        }
        Err(crate::paths::CommandTimeoutError::Io(e)) => {
            return Err(AuthError(format!(
                "Could not invoke macOS Keychain CLI (`security`): {e}"
            )));
        }
    };
    let pw = String::from_utf8_lossy(&output.stdout)
        .trim_end_matches(&['\r', '\n'][..])
        .to_string();
    if !output.status.success() || pw.is_empty() {
        return Err(AuthError(
            "Could not read 'Microsoft Teams Safe Storage' from macOS Keychain. \
             Ensure Microsoft Teams is signed in and Keychain access is allowed."
                .to_string(),
        ));
    }
    let key = derive_aes_key(&pw);
    if let Ok(mut guard) = DERIVED_KEY_CACHE.lock() {
        *guard = Some(key);
    }
    Ok(key)
}

pub fn clear_keychain_cache() {
    if let Ok(mut guard) = DERIVED_KEY_CACHE.lock() {
        *guard = None;
    }
}

pub fn is_expired(expires_utc: i64, now_ts: f64) -> bool {
    if expires_utc <= 0 {
        return false;
    }
    let unix_s = (expires_utc as f64 / 1_000_000.0) - WEBKIT_EPOCH_OFFSET_SEC;
    unix_s <= now_ts
}

pub fn now_unix_f64() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

type RawCookieRow = (String, String, String, Vec<u8>, String, i64, bool);

pub fn load_cookies(
    db_path: Option<&Path>,
    password: Option<&str>,
    now_opt: Option<f64>,
) -> Result<Vec<Cookie>, AuthError> {
    let src = db_path.map(|p| p.to_path_buf()).unwrap_or_else(cookies_path);
    if !src.is_file() {
        return Err(AuthError(format!(
            "Teams cookie store not found at {}. Is the new Microsoft Teams installed and signed in?",
            src.display()
        )));
    }
    let now_ts = now_opt.unwrap_or_else(now_unix_f64);
    let mut db_version = 0i64;
    let mut raw_rows: Vec<RawCookieRow> = Vec::new();
    let mut last_err: Option<String> = None;

    for attempt in 0..COOKIE_COPY_ATTEMPTS {
        let td = tempfile::Builder::new()
            .prefix("teams-cli-cookies-")
            .tempdir()
            .map_err(|e| AuthError(e.to_string()))?;
        let _ = fs::set_permissions(td.path(), fs::Permissions::from_mode(0o700));
        let res = (|| -> Result<(i64, Vec<RawCookieRow>), String> {
            let name = src.file_name().unwrap();
            let copied = td.path().join(name);
            fs::copy(&src, &copied).map_err(|e| e.to_string())?;
            let _ = fs::set_permissions(&copied, fs::Permissions::from_mode(0o600));
            let wal = PathBuf::from(format!("{}-wal", src.display()));
            if wal.is_file() {
                let copied_wal = td.path().join(format!("{}-wal", name.to_string_lossy()));
                fs::copy(&wal, &copied_wal).map_err(|e| format!("copying cookie WAL {}: {e}", wal.display()))?;
                let _ = fs::set_permissions(&copied_wal, fs::Permissions::from_mode(0o600));
            }
            let conn =
                Connection::open_with_flags(&copied, OpenFlags::SQLITE_OPEN_READ_WRITE).map_err(|e| e.to_string())?;
            let _ = conn.execute_batch("PRAGMA busy_timeout = 10000;");
            let ver: i64 = conn
                .query_row("SELECT value FROM meta WHERE key = 'version'", [], |r| {
                    let s: String = r.get(0)?;
                    Ok(s.parse::<i64>().unwrap_or(0))
                })
                .unwrap_or(0);
            let mut has_part = false;
            if let Ok(mut st) = conn.prepare("PRAGMA table_info(cookies)") {
                if let Ok(iter) = st.query_map([], |r| r.get::<_, String>(1)) {
                    for c in iter.flatten() {
                        if c == "top_frame_site_key" {
                            has_part = true;
                        }
                    }
                }
            }
            let sql = if has_part {
                "SELECT host_key, name, value, encrypted_value, path, expires_utc, \
                        (COALESCE(top_frame_site_key, '') != '') AS partitioned \
                 FROM cookies ORDER BY (COALESCE(top_frame_site_key, '') != '') ASC"
            } else {
                "SELECT host_key, name, value, encrypted_value, path, expires_utc, 0 AS partitioned FROM cookies"
            };
            let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                        r.get::<_, Option<Vec<u8>>>(3)?.unwrap_or_default(),
                        r.get::<_, Option<String>>(4)?.unwrap_or_else(|| "/".to_string()),
                        r.get::<_, Option<i64>>(5)?.unwrap_or(0),
                        r.get::<_, i64>(6)? != 0,
                    ))
                })
                .map_err(|e| e.to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())?;
            Ok((ver, rows))
        })();
        match res {
            Ok((ver, rows)) => {
                db_version = ver;
                raw_rows = rows;
                last_err = None;
                break;
            }
            Err(e) => {
                last_err = Some(e);
                if attempt + 1 < COOKIE_COPY_ATTEMPTS {
                    thread::sleep(Duration::from_millis(150));
                }
            }
        }
    }
    if let Some(err) = last_err {
        return Err(AuthError(format!("Could not read Teams cookie database: {err}")));
    }

    let mut aes_key: Option<[u8; 16]> = None;
    let mut cookies = Vec::new();
    let mut v10_seen = 0usize;
    let mut v10_ok = 0usize;

    for (host_key, name, plain, enc, path, exp, partitioned) in raw_rows {
        if is_expired(exp, now_ts) {
            continue;
        }
        let val = if enc.is_empty() {
            plain
        } else if enc.starts_with(b"v10") {
            v10_seen += 1;
            let key = match aes_key {
                Some(k) => k,
                None => {
                    let k = match password {
                        Some(p) => derive_aes_key(p),
                        None => get_keychain_aes_key()?,
                    };
                    aes_key = Some(k);
                    k
                }
            };
            match decrypt_cookie_value(&enc, &host_key, &key, db_version, &plain) {
                Ok(v) => {
                    v10_ok += 1;
                    v
                }
                Err(_) => continue,
            }
        } else if !plain.is_empty() {
            plain
        } else {
            continue;
        };
        cookies.push(Cookie {
            host_key,
            name,
            value: val,
            path: if path.is_empty() { "/".to_string() } else { path },
            expires_utc: exp,
            partitioned,
        });
    }
    if v10_seen > 0 && v10_ok == 0 {
        return Err(AuthError(
            "Failed to decrypt any Teams cookies. \
             Check Keychain access or reopen Microsoft Teams to refresh cookies."
                .to_string(),
        ));
    }
    Ok(cookies)
}

pub fn decode_jwt_payload(jwt: &str) -> Result<Value, String> {
    let parts: Vec<&str> = jwt.trim().split('.').collect();
    if parts.len() != 3 || parts[0].is_empty() || parts[1].is_empty() {
        return Err("not a 3-segment JWT".to_string());
    }
    let hdr_raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[0])
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(parts[0]))
        .map_err(|_| "invalid JWT header base64".to_string())?;
    let hdr: Value = serde_json::from_slice(&hdr_raw).map_err(|_| "invalid JWT header JSON".to_string())?;
    if !hdr.is_object() {
        return Err("JWT header is not a JSON object".to_string());
    }
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1])
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(parts[1]))
        .map_err(|_| "invalid JWT base64".to_string())?;
    let val: Value = serde_json::from_slice(&raw).map_err(|_| "invalid JWT JSON".to_string())?;
    if !val.is_object() {
        return Err("JWT payload is not a JSON object".to_string());
    }
    Ok(val)
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

pub fn urlencoding_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn is_trusted_host(host: Option<&str>) -> bool {
    let h = host.unwrap_or("").trim().to_ascii_lowercase();
    let h = h.trim_end_matches('.');
    if h.is_empty() {
        return false;
    }
    for suffix in TRUSTED_HOST_SUFFIXES {
        let base = &suffix[1..];
        if h == base || h.ends_with(suffix) {
            return true;
        }
    }
    false
}

pub fn is_trusted_url(url_str: &str) -> bool {
    let Ok(u) = Url::parse(url_str) else {
        return false;
    };
    u.scheme().eq_ignore_ascii_case("https") && is_trusted_host(u.host_str())
}

pub fn is_login_host(host: Option<&str>) -> bool {
    let h = host.unwrap_or("").trim().to_ascii_lowercase();
    let h = h.trim_end_matches('.');
    h == "login.microsoftonline.com" || h == "login.live.com" || h.ends_with(".login.microsoftonline.com")
}

pub fn cookie_matches_host(host_key: &str, req_host: &str) -> bool {
    domain_matches(host_key, req_host)
}

fn domain_matches(host_key: &str, req_host: &str) -> bool {
    let req = req_host.trim().to_ascii_lowercase();
    let req = req.trim_end_matches('.');
    let hk = host_key.trim().to_ascii_lowercase();
    let hk = hk.trim_end_matches('.');
    if req.is_empty() || hk.is_empty() {
        return false;
    }
    if let Some(domain) = hk.strip_prefix('.') {
        let domain = domain.trim_start_matches('.');
        !domain.is_empty() && (req == domain || req.ends_with(&format!(".{domain}")))
    } else {
        req == hk
    }
}

fn path_matches(cookie_path: &str, req_path: &str) -> bool {
    let cp = if cookie_path.is_empty() { "/" } else { cookie_path };
    let rp = if req_path.is_empty() { "/" } else { req_path };
    if rp == cp {
        return true;
    }
    if let Some(rest) = rp.strip_prefix(cp) {
        if cp.ends_with('/') || rest.starts_with('/') {
            return true;
        }
    }
    false
}

pub fn build_cookie_header(cookies: &[Cookie], url_str: &str, now_opt: Option<f64>) -> String {
    if !is_trusted_url(url_str) {
        return String::new();
    }
    let Ok(u) = Url::parse(url_str) else {
        return String::new();
    };
    let host = u.host_str().unwrap_or("").to_ascii_lowercase();
    let req_path = u.path();
    let now_ts = now_opt.unwrap_or_else(now_unix_f64);

    let mut matched: Vec<&Cookie> = cookies
        .iter()
        .filter(|c| {
            !is_expired(c.expires_utc, now_ts) && domain_matches(&c.host_key, &host) && path_matches(&c.path, req_path)
        })
        .collect();
    matched.sort_by(|a, b| {
        a.partitioned
            .cmp(&b.partitioned)
            .then_with(|| b.path.len().cmp(&a.path.len()))
            .then_with(|| a.host_key.starts_with('.').cmp(&b.host_key.starts_with('.')))
            .then_with(|| b.host_key.len().cmp(&a.host_key.len()))
    });

    let mut seen = HashSet::new();
    let mut parts = Vec::new();
    for c in matched {
        let bad = |s: &str| s.contains(['\r', '\n', '\0']);
        if bad(&c.name) || bad(&c.value) {
            continue;
        }
        if seen.insert(&c.name) {
            parts.push(format!("{}={}", c.name, c.value));
        }
    }
    parts.join("; ")
}
