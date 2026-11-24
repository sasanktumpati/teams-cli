use crate::formatters::Quoted;
use crate::identity;
use chrono::{DateTime, Days, Duration, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

static REL_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^\s*(\d+)\s*(m|min|h|d|w)\s*$").unwrap());
static EXPIRY_REL_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^\s*(\d+)\s*(m|min|h|d|w|y)\s*$").unwrap());
static DATE_ONLY_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d{4}-\d{2}-\d{2}$").unwrap());
static ISO_FRAC_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\.\d{6})\d+").unwrap());
static NUM_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d+(\.\d+)?$").unwrap());

pub fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

pub fn parse_when(value: Option<&str>, end: bool) -> Result<Option<i64>, String> {
    parse_when_at(value, end, Local::now())
}

pub fn parse_expiry(value: Option<&str>) -> Result<Option<i64>, String> {
    parse_expiry_at(value, Local::now())
}

pub fn parse_expiry_at(value: Option<&str>, now: DateTime<Local>) -> Result<Option<i64>, String> {
    const MAX_EXPIRY_MS: i64 = 100 * 365 * 86_400 * 1000;
    let Some(raw) = value else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("never") {
        return Ok(None);
    }
    let now_ms = now.timestamp_millis();
    let ts = if let Some(caps) = EXPIRY_REL_RE.captures(trimmed) {
        let n: i64 = caps[1].parse().map_err(|_| bad_expiry_msg(raw))?;
        if n <= 0 {
            return Err(format!("Expiry {} must be greater than 0.", Quoted(raw)));
        }
        let unit_secs: i64 = match caps[2].to_ascii_lowercase().as_str() {
            "m" | "min" => 60,
            "h" => 3600,
            "d" => 86_400,
            "w" => 7 * 86_400,
            "y" => 365 * 86_400,
            _ => return Err(bad_expiry_msg(raw)),
        };
        let delta_ms = n
            .checked_mul(unit_secs)
            .and_then(|s| s.checked_mul(1000))
            .ok_or_else(|| bad_expiry_msg(raw))?;
        now_ms.checked_add(delta_ms).ok_or_else(|| bad_expiry_msg(raw))?
    } else {
        parse_when_at(Some(trimmed), true, now)
            .map_err(|_| bad_expiry_msg(raw))?
            .ok_or_else(|| bad_expiry_msg(raw))?
    };
    if ts <= now_ms {
        return Err(format!("Expiry {} must be in the future.", Quoted(raw)));
    }
    if ts - now_ms > MAX_EXPIRY_MS {
        return Err("Expiry must be at most 100 years in the future.".to_string());
    }
    Ok(Some(ts))
}

fn bad_expiry_msg(value: &str) -> String {
    format!(
        "Can't read expiry {}. Use e.g. 30m, 12h, 7d, 30d, 1y, 2027-01-01, or never.",
        Quoted(value)
    )
}

pub fn parse_when_at(value: Option<&str>, end: bool, now: DateTime<Local>) -> Result<Option<i64>, String> {
    let Some(raw) = value else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let v = trimmed.to_ascii_lowercase();
    if let Some(caps) = REL_RE.captures(&v) {
        let n: i64 = caps[1].parse().map_err(|_| bad_time_msg(raw))?;
        let unit: i64 = match &caps[2] {
            "m" | "min" => 60,
            "h" => 3600,
            "d" => 86_400,
            "w" => 7 * 86_400,
            _ => return Err(bad_time_msg(raw)),
        };
        let delta_ms = n
            .checked_mul(unit)
            .and_then(|s| s.checked_mul(1000))
            .ok_or_else(|| bad_time_msg(raw))?;
        let ts = now
            .timestamp_millis()
            .checked_sub(delta_ms)
            .ok_or_else(|| bad_time_msg(raw))?;
        return Ok(Some(ts));
    }
    if v == "today" || v == "yesterday" {
        let mut day = now.date_naive();
        if v == "yesterday" {
            day = day.checked_sub_days(Days::new(1)).ok_or_else(|| bad_time_msg(raw))?;
        }
        if end {
            day = day.checked_add_days(Days::new(1)).ok_or_else(|| bad_time_msg(raw))?;
        }
        let ms = local_midnight_ms(day).ok_or_else(|| bad_time_msg(raw))?;
        return Ok(Some(ms));
    }
    if v.len() >= 12 && v.chars().all(|c| c.is_ascii_digit()) {
        if let Ok(ms_val) = v.parse::<i64>() {
            return Ok(Some(ms_val));
        }
    }

    let normalized = if trimmed.ends_with('z') || trimmed.ends_with('Z') {
        format!("{}+00:00", &trimmed[..trimmed.len() - 1])
    } else {
        trimmed.to_string()
    };
    if let Some(ms) = parse_iso_with_offset(&normalized) {
        return Ok(Some(ms));
    }
    if DATE_ONLY_RE.is_match(trimmed) {
        if let Ok(d) = NaiveDate::parse_from_str(trimmed, "%Y-%m-%d") {
            let day = if end { d.checked_add_days(Days::new(1)) } else { Some(d) };
            if let Some(ms) = day.and_then(local_midnight_ms) {
                return Ok(Some(ms));
            }
        }
    }
    for fmt in &[
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(ndt) = NaiveDateTime::parse_from_str(trimmed, fmt) {
            if let Some(local_dt) = Local.from_local_datetime(&ndt).earliest() {
                return Ok(Some(local_dt.timestamp_millis()));
            }
        }
    }
    Err(bad_time_msg(raw))
}

pub fn local_midnight_ms(day: NaiveDate) -> Option<i64> {
    first_valid_local_ms(&Local, day)
}

pub fn first_valid_local_ms<Tz: TimeZone>(tz: &Tz, day: NaiveDate) -> Option<i64> {
    let midnight = day.and_hms_opt(0, 0, 0)?;
    (0..=120).find_map(|min| {
        tz.from_local_datetime(&(midnight + Duration::minutes(min)))
            .earliest()
            .map(|dt| dt.timestamp_millis())
    })
}

fn bad_time_msg(value: &str) -> String {
    format!(
        "Can't read time {}. Use e.g. 30m, 12h, 7d, 2w, today, yesterday, 2026-11-01 or 2026-11-01T14:00.",
        Quoted(value)
    )
}

fn parse_iso_with_offset(s: &str) -> Option<i64> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp_millis());
    }
    for fmt in &[
        "%Y-%m-%dT%H:%M%:z",
        "%Y-%m-%d %H:%M%:z",
        "%Y-%m-%dT%H:%M:%S%.f%z",
        "%Y-%m-%d %H:%M:%S%.f%z",
        "%Y-%m-%dT%H:%M%z",
        "%Y-%m-%d %H:%M%z",
    ] {
        if let Ok(dt) = DateTime::parse_from_str(s, fmt) {
            return Some(dt.timestamp_millis());
        }
    }
    None
}

pub fn parse_iso_ms(value: &str) -> Option<i64> {
    let s = value.trim();
    if s.is_empty() {
        return None;
    }
    let s = ISO_FRAC_RE.replace_all(s, "$1");
    let s = if s.ends_with('Z') || s.ends_with('z') {
        format!("{}+00:00", &s[..s.len() - 1])
    } else {
        s.into_owned()
    };
    if let Some(ms) = parse_iso_with_offset(&s) {
        return Some(ms);
    }
    for fmt in &[
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d",
    ] {
        if let Ok(ndt) = NaiveDateTime::parse_from_str(&s, fmt) {
            return Some(Utc.from_utc_datetime(&ndt).timestamp_millis());
        }
        if *fmt == "%Y-%m-%d" {
            if let Ok(d) = NaiveDate::parse_from_str(&s, fmt) {
                return Some(
                    Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).unwrap())
                        .timestamp_millis(),
                );
            }
        }
    }
    None
}

pub fn to_ms(v: &Value) -> Option<i64> {
    match v {
        Value::Null | Value::Bool(_) => None,
        Value::Number(_) => identity::ms(v),
        Value::String(s) => {
            let trimmed = s.trim();
            if NUM_RE.is_match(trimmed) {
                identity::ms_str(trimmed)
            } else {
                parse_iso_ms(trimmed)
            }
        }
        _ => None,
    }
}

pub fn iso(ms_val: Option<i64>) -> Option<String> {
    let ms = ms_val.filter(|&m| m != 0)?;
    let dt = Local.timestamp_millis_opt(ms).single()?;
    Some(dt.format("%Y-%m-%dT%H:%M:%S%:z").to_string())
}

pub fn short(ms_val: Option<i64>) -> String {
    match ms_val
        .filter(|&m| m != 0)
        .and_then(|m| Local.timestamp_millis_opt(m).single())
    {
        Some(dt) => dt.format("%Y-%m-%d %H:%M").to_string(),
        None => "????-??-?? ??:??".to_string(),
    }
}

pub fn day(ms_val: Option<i64>) -> String {
    match ms_val
        .filter(|&m| m != 0)
        .and_then(|m| Local.timestamp_millis_opt(m).single())
    {
        Some(dt) => dt.format("%Y-%m-%d").to_string(),
        None => "Unknown date".to_string(),
    }
}

pub fn hhmm(ms_val: Option<i64>) -> String {
    match ms_val
        .filter(|&m| m != 0)
        .and_then(|m| Local.timestamp_millis_opt(m).single())
    {
        Some(dt) => dt.format("%H:%M").to_string(),
        None => "??:??".to_string(),
    }
}
