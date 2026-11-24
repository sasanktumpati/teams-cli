use crate::json_codec;
use crate::paths;
use crate::timeutil;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const API_TOKEN_ENV: &str = "TEAMS_CLI_API_TOKEN";
pub(crate) const UNAUTHORIZED_MESSAGE: &str =
    "Missing or invalid bearer token. Run `teams token` on this Mac to print it.";

const MAX_NAMED_TOKEN_NAME: usize = 64;
const NAMED_TOKEN_PREFIX: &str = "tc_";

pub fn extract_host(header: Option<&str>) -> String {
    let h = header.unwrap_or("").trim();
    if let Some(after) = h.strip_prefix('[') {
        let Some((inside, rest)) = after.split_once(']') else {
            return String::new();
        };
        if !rest.is_empty()
            && !(rest.starts_with(':') && rest[1..].chars().all(|c| c.is_ascii_digit()) && rest.len() > 1)
        {
            return String::new();
        }
        return inside.to_ascii_lowercase();
    }
    match h.rsplit_once(':') {
        Some((host, port)) => {
            if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
                return String::new();
            }
            host.to_ascii_lowercase()
        }
        None => h.to_ascii_lowercase(),
    }
}

pub fn is_allowed(host: Option<&str>, origin: Option<&str>, sec_fetch_site: Option<&str>) -> bool {
    let h = extract_host(host);
    let local_host = matches!(h.as_str(), "127.0.0.1" | "localhost" | "::1");
    let has_origin = origin.is_some_and(|o| !o.is_empty());
    let sfs = sec_fetch_site.unwrap_or("").trim().to_ascii_lowercase();
    let bad_sfs = !sfs.is_empty() && sfs != "none";
    local_host && !has_origin && !bad_sfs
}

pub fn is_allowed_headers(hosts: &[&str], origins: &[&str], sec_fetch_sites: &[&str]) -> bool {
    let [host] = hosts else {
        return false;
    };
    if origins.len() > 1 || origins.iter().any(|o| !o.is_empty()) {
        return false;
    }
    if !sec_fetch_sites.iter().all(|s| is_allowed(Some(host), None, Some(s))) {
        return false;
    }
    is_allowed(Some(host), None, None)
}

pub fn env_api_token() -> Option<String> {
    env::var(API_TOKEN_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn random_hex(bytes: usize) -> Result<String, String> {
    let mut buf = vec![0u8; bytes];
    getrandom::getrandom(&mut buf).map_err(|e| format!("OS random number generator failed: {e}"))?;
    Ok(hex::encode(buf))
}

fn generate_api_token() -> Result<String, String> {
    random_hex(32)
}

fn sha256_bytes(s: &str) -> [u8; 32] {
    let d = Sha256::digest(s.as_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(&d);
    out
}

fn ct_eq_32(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

type FileHandleSig = (PathBuf, u64, u64, i64, i64, u64);

fn sig_from_meta(path: &Path, m: &fs::Metadata) -> FileHandleSig {
    (path.to_path_buf(), m.dev(), m.ino(), m.mtime(), m.mtime_nsec(), m.len())
}

fn path_sig(path: &Path) -> Option<FileHandleSig> {
    let m = fs::metadata(path).ok()?;
    Some(sig_from_meta(path, &m))
}

fn read_private_file_with_sig(path: &Path) -> Result<Option<(FileHandleSig, String)>, String> {
    let mut f = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("failed reading {}: {e}", path.display())),
    };
    let meta = f
        .metadata()
        .map_err(|e| format!("failed stating {}: {e}", path.display()))?;
    if meta.permissions().mode() & 0o777 != 0o600 {
        let _ = f.set_permissions(fs::Permissions::from_mode(0o600));
    }
    let sig = sig_from_meta(path, &meta);
    let mut content = String::new();
    f.read_to_string(&mut content)
        .map_err(|e| format!("failed reading {}: {e}", path.display()))?;
    Ok(Some((sig, content)))
}

fn read_api_token_file(path: &Path) -> Result<Option<String>, String> {
    match read_private_file_with_sig(path)? {
        Some((_, s)) => {
            let t = s.trim();
            Ok((!t.is_empty()).then(|| t.to_string()))
        }
        None => Ok(None),
    }
}

fn create_api_token_file(path: &Path, token: &str) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let mut rnd = [0u8; 6];
    getrandom::getrandom(&mut rnd).map_err(|e| std::io::Error::other(e.to_string()))?;
    let tmp = parent.join(format!(".api.token.{}.{}.tmp", std::process::id(), hex::encode(rnd)));
    let res = (|| {
        let mut f = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        f.write_all(format!("{token}\n").as_bytes())?;
        f.sync_all()?;
        drop(f);
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        fs::hard_link(&tmp, path)?;
        paths::fsync_dir(parent);
        Ok(())
    })();
    let _ = fs::remove_file(&tmp);
    res
}

pub fn peek_api_token() -> Result<Option<String>, String> {
    if let Some(t) = env_api_token() {
        return Ok(Some(t));
    }
    read_api_token_file(&paths::api_token_path())
}

pub fn load_or_create_api_token() -> Result<String, String> {
    if let Some(t) = env_api_token() {
        return Ok(t);
    }
    let path = paths::api_token_path();
    for _ in 0..5 {
        if let Some(t) = read_api_token_file(&path)? {
            return Ok(t);
        }
        let token = generate_api_token()?;
        if path.exists() {
            paths::atomic_write_text(&path, &format!("{token}\n"))
                .map_err(|e| format!("failed writing {}: {e}", path.display()))?;
            return Ok(token);
        }
        match create_api_token_file(&path, &token) {
            Ok(()) => return Ok(token),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("failed writing {}: {e}", path.display())),
        }
    }
    Err(format!("failed creating {}: it keeps changing", path.display()))
}

pub fn rotate_api_token() -> Result<String, String> {
    let path = paths::api_token_path();
    let token = generate_api_token()?;
    paths::atomic_write_text(&path, &format!("{token}\n"))
        .map_err(|e| format!("failed writing {}: {e}", path.display()))?;
    Ok(token)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredNamedToken {
    pub id: String,
    pub name: String,
    pub prefix: String,
    pub sha256: String,
    pub created_at_ms: i64,
    #[serde(default)]
    pub expires_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TokenStoreFile {
    version: u32,
    tokens: Vec<StoredNamedToken>,
}

fn validate_token_name(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("Token name cannot be empty.".to_string());
    }
    if name.len() > MAX_NAMED_TOKEN_NAME {
        return Err(format!("Token name must be at most {MAX_NAMED_TOKEN_NAME} characters."));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ' '))
    {
        return Err("Token name may only contain letters, digits, spaces, '.', '-' and '_'.".to_string());
    }
    if name.len() == 8 && name.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("Token name cannot be an 8-character hex string (reserved for token ids).".to_string());
    }
    Ok(name.to_string())
}

fn lock_tokens_file() -> Result<File, String> {
    let path = paths::home().join(".api.tokens.lock");
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .map_err(|e| format!("failed opening token lock {}: {e}", path.display()))?;
    f.lock().map_err(|e| format!("failed locking token store: {e}"))?;
    Ok(f)
}

fn read_named_store(path: &Path) -> Result<(Option<FileHandleSig>, Vec<StoredNamedToken>), String> {
    let Some((sig, content)) = read_private_file_with_sig(path)? else {
        return Ok((None, Vec::new()));
    };
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Ok((Some(sig), Vec::new()));
    }
    let parsed: TokenStoreFile =
        serde_json::from_str(trimmed).map_err(|e| format!("corrupt {}: {e}", path.display()))?;
    Ok((Some(sig), parsed.tokens))
}

fn write_named_store(path: &Path, tokens: &[StoredNamedToken]) -> Result<(), String> {
    let payload = TokenStoreFile {
        version: 1,
        tokens: tokens.to_vec(),
    };
    let text = format!("{}\n", json_codec::to_pretty(&payload));
    paths::atomic_write_text(path, &text).map_err(|e| format!("failed writing {}: {e}", path.display()))
}

fn token_metadata_json(t: &StoredNamedToken, now_ms: i64) -> Value {
    let expired = t.expires_at_ms.is_some_and(|exp| exp <= now_ms);
    json!({
        "id": t.id,
        "name": t.name,
        "prefix": t.prefix,
        "createdAt": timeutil::iso(Some(t.created_at_ms)),
        "createdAtMs": t.created_at_ms,
        "expiresAt": t.expires_at_ms.and_then(|e| timeutil::iso(Some(e))),
        "expiresAtMs": t.expires_at_ms,
        "expired": expired,
    })
}

pub fn create_named_token(name: &str, expires: Option<&str>) -> Result<Value, String> {
    let clean_name = validate_token_name(name)?;
    let expires_at_ms = timeutil::parse_expiry(expires)?;
    let _lock = lock_tokens_file()?;
    let path = paths::api_tokens_path();
    let (_, mut tokens) = read_named_store(&path)?;
    let now_ms = timeutil::now_ms();

    if tokens
        .iter()
        .any(|t| t.name.eq_ignore_ascii_case(&clean_name) && !t.expires_at_ms.is_some_and(|exp| exp <= now_ms))
    {
        return Err(format!(
            "An active token named '{clean_name}' already exists. Revoke it first or pick another name."
        ));
    }
    tokens.retain(|t| !(t.name.eq_ignore_ascii_case(&clean_name) && t.expires_at_ms.is_some_and(|exp| exp <= now_ms)));

    let id = random_hex(4)?;
    let secret = format!("{NAMED_TOKEN_PREFIX}{}", generate_api_token()?);
    let prefix = secret.chars().take(11).collect::<String>();
    let hash_hex = hex::encode(sha256_bytes(&secret));

    let entry = StoredNamedToken {
        id: id.clone(),
        name: clean_name,
        prefix,
        sha256: hash_hex,
        created_at_ms: now_ms,
        expires_at_ms,
    };
    tokens.push(entry.clone());
    write_named_store(&path, &tokens)?;

    let mut out = token_metadata_json(&entry, now_ms);
    if let Some(obj) = out.as_object_mut() {
        obj.insert("token".into(), Value::String(secret));
        obj.insert("path".into(), Value::String(path.to_string_lossy().into_owned()));
    }
    Ok(out)
}

pub fn list_named_tokens(include_expired: bool) -> Result<Value, String> {
    let path = paths::api_tokens_path();
    let (_, tokens) = read_named_store(&path)?;
    let now_ms = timeutil::now_ms();
    let items: Vec<Value> = tokens
        .iter()
        .filter(|t| include_expired || !t.expires_at_ms.is_some_and(|exp| exp <= now_ms))
        .map(|t| token_metadata_json(t, now_ms))
        .collect();
    Ok(json!({
        "tokens": items,
        "count": items.len(),
        "path": path.to_string_lossy(),
    }))
}

pub fn revoke_named_token(name_or_id: &str) -> Result<Value, String> {
    let needle = name_or_id.trim();
    if needle.is_empty() {
        return Err("Provide a token name or id to revoke.".to_string());
    }
    let _lock = lock_tokens_file()?;
    let path = paths::api_tokens_path();
    let (_, mut tokens) = read_named_store(&path)?;
    let now_ms = timeutil::now_ms();

    let id_matches: Vec<usize> = tokens
        .iter()
        .enumerate()
        .filter(|(_, t)| t.id == needle)
        .map(|(i, _)| i)
        .collect();
    let name_matches: Vec<usize> = tokens
        .iter()
        .enumerate()
        .filter(|(_, t)| t.name.eq_ignore_ascii_case(needle))
        .map(|(i, _)| i)
        .collect();
    let matched_indices = if !id_matches.is_empty() && !name_matches.is_empty() && id_matches != name_matches {
        return Err(format!(
            "'{needle}' matches both a token id and another token's name; pass the exact 8-character id."
        ));
    } else if !id_matches.is_empty() {
        id_matches
    } else if name_matches.len() > 1 {
        let active: Vec<usize> = name_matches
            .iter()
            .copied()
            .filter(|&i| !tokens[i].expires_at_ms.is_some_and(|exp| exp <= now_ms))
            .collect();
        if active.len() == 1 {
            active
        } else {
            name_matches
        }
    } else {
        name_matches
    };
    if matched_indices.is_empty() {
        return Err(format!(
            "No token matches '{needle}'. Run `teams token list` to inspect tokens."
        ));
    }
    if matched_indices.len() > 1 {
        let ids: Vec<&str> = matched_indices.iter().map(|&i| tokens[i].id.as_str()).collect();
        return Err(format!(
            "Multiple tokens match '{needle}' (ids: {}). Revoke by exact id.",
            ids.join(", ")
        ));
    }
    let removed = tokens.remove(matched_indices[0]);
    write_named_store(&path, &tokens)?;
    let mut out = token_metadata_json(&removed, now_ms);
    if let Some(obj) = out.as_object_mut() {
        obj.insert("revoked".into(), Value::Bool(true));
    }
    Ok(out)
}

pub fn prune_expired_tokens() -> Result<Value, String> {
    let _lock = lock_tokens_file()?;
    let path = paths::api_tokens_path();
    let (_, mut tokens) = read_named_store(&path)?;
    let now_ms = timeutil::now_ms();
    let before = tokens.len();
    tokens.retain(|t| !t.expires_at_ms.is_some_and(|exp| exp <= now_ms));
    let removed = before - tokens.len();
    if removed > 0 {
        write_named_store(&path, &tokens)?;
    }
    Ok(json!({
        "pruned": removed,
        "remaining": tokens.len(),
        "path": path.to_string_lossy(),
    }))
}

type NamedDigest = ([u8; 32], Option<i64>);
type TokenSnapshot = ([u8; 32], Vec<NamedDigest>);

struct CachedTokens {
    primary: Option<(FileHandleSig, [u8; 32])>,
    named_sig: Option<FileHandleSig>,
    named: Vec<NamedDigest>,
}

pub(crate) struct ApiToken {
    cached: Mutex<CachedTokens>,
}

impl Default for ApiToken {
    fn default() -> Self {
        Self {
            cached: Mutex::new(CachedTokens {
                primary: None,
                named_sig: None,
                named: Vec::new(),
            }),
        }
    }
}

impl ApiToken {
    pub(crate) fn ensure_ready(&self) -> Result<(), String> {
        if env_api_token().is_some() {
            return Ok(());
        }
        let _ = load_or_create_api_token()?;
        let _ = self.refresh_if_needed()?;
        Ok(())
    }

    fn refresh_if_needed(&self) -> Result<TokenSnapshot, String> {
        let primary_path = paths::api_token_path();
        let named_path = paths::api_tokens_path();
        let mut cache = self.cached.lock().unwrap_or_else(|p| p.into_inner());

        let cur_psig = path_sig(&primary_path);
        let primary_digest = match (&cur_psig, &cache.primary) {
            (Some(s), Some((cs, digest))) if s == cs => *digest,
            _ => match read_private_file_with_sig(&primary_path)? {
                Some((sig, content)) if !content.trim().is_empty() => {
                    let digest = sha256_bytes(content.trim());
                    cache.primary = Some((sig, digest));
                    digest
                }
                _ => {
                    let tok = load_or_create_api_token()?;
                    let digest = sha256_bytes(&tok);
                    if let Some((sig, _)) = read_private_file_with_sig(&primary_path)? {
                        cache.primary = Some((sig, digest));
                    } else {
                        cache.primary = None;
                    }
                    digest
                }
            },
        };

        let cur_nsig = path_sig(&named_path);
        if cur_nsig != cache.named_sig {
            match read_named_store(&named_path) {
                Ok((sig, list)) => {
                    let mut digests = Vec::with_capacity(list.len());
                    for t in list {
                        if let Ok(raw) = hex::decode(&t.sha256) {
                            if raw.len() == 32 {
                                let mut arr = [0u8; 32];
                                arr.copy_from_slice(&raw);
                                digests.push((arr, t.expires_at_ms));
                            }
                        }
                    }
                    cache.named_sig = sig;
                    cache.named = digests;
                }
                Err(_) => {
                    cache.named_sig = cur_nsig;
                    cache.named.clear();
                }
            }
        }

        Ok((primary_digest, cache.named.clone()))
    }

    pub(crate) fn verify(&self, candidate: &str) -> Result<bool, String> {
        let given = sha256_bytes(candidate);
        if let Some(env_tok) = env_api_token() {
            let expected = sha256_bytes(&env_tok);
            return Ok(ct_eq_32(&given, &expected));
        }
        let (primary_digest, named) = self.refresh_if_needed()?;
        let now_ms = timeutil::now_ms();
        let mut matched = ct_eq_32(&given, &primary_digest);
        for (digest, expires_at_ms) in &named {
            let active = !expires_at_ms.is_some_and(|exp| exp <= now_ms);
            let eq = ct_eq_32(&given, digest);
            matched |= active & eq;
        }
        Ok(matched)
    }
}

pub fn bearer_token<'a>(authorizations: &[&'a str]) -> Option<&'a str> {
    let [value] = authorizations else {
        return None;
    };
    let v = value.trim();
    let (scheme, rest) = v.split_at_checked(6)?;
    if !scheme.eq_ignore_ascii_case("bearer") || !rest.starts_with([' ', '\t']) {
        return None;
    }
    let token = rest.trim();
    (!token.is_empty() && !token.contains([' ', '\t'])).then_some(token)
}
