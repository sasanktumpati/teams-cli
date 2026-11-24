use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

pub const DAY_MS: i64 = 86_400_000;
pub const STALE_SOURCE_DAYS: i64 = 3;

pub static ONE_ON_ONE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^19:([0-9a-fA-F-]{36})_([0-9a-fA-F-]{36})@").unwrap());
static TRAILING_ZERO_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^-?\d+\.0+$").unwrap());

pub fn oid(mri: &str) -> String {
    if mri.is_empty() {
        return String::new();
    }
    mri.rsplit(':').next().unwrap_or("").trim().to_ascii_lowercase()
}

pub fn valid_oid(mri: &str) -> Option<String> {
    let tail = oid(mri);
    if tail.len() >= 8 {
        Some(tail)
    } else {
        None
    }
}

pub fn same_person(a: Option<&str>, b: Option<&str>) -> bool {
    let (Some(a), Some(b)) = (a, b) else {
        return false;
    };
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a == b {
        return true;
    }
    let (oa, ob) = (oid(a), oid(b));
    !oa.is_empty() && oa == ob
}

pub fn one_on_one_oids(cid: &str) -> Vec<String> {
    if !cid.starts_with("19:") {
        return Vec::new();
    }
    let Some(at) = cid.find('@') else {
        return Vec::new();
    };
    let body = &cid[3..at];
    let parts: Vec<&str> = body.split('_').collect();
    if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
        return Vec::new();
    }
    vec![parts[0].to_ascii_lowercase(), parts[1].to_ascii_lowercase()]
}

pub fn clean_id(v: &Value) -> Option<String> {
    match v {
        Value::Null | Value::Bool(_) => None,
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(i.to_string())
            } else if let Some(u) = n.as_u64() {
                Some(u.to_string())
            } else if let Some(f) = n.as_f64() {
                if f.fract() == 0.0 && f.is_finite() {
                    Some((f as i64).to_string())
                } else {
                    Some(f.to_string())
                }
            } else {
                None
            }
        }
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return None;
            }
            if TRAILING_ZERO_RE.is_match(trimmed) {
                Some(trimmed.split('.').next().unwrap_or(trimmed).to_string())
            } else {
                Some(trimmed.to_string())
            }
        }
        _ => None,
    }
}

pub fn ms(v: &Value) -> Option<i64> {
    let f = match v {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => s.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    if f > 0.0 && f < 4e12 {
        Some(f as i64)
    } else {
        None
    }
}

pub fn ms_str(s: &str) -> Option<i64> {
    let f = s.trim().parse::<f64>().ok()?;
    if f > 0.0 && f < 4e12 {
        Some(f as i64)
    } else {
        None
    }
}
