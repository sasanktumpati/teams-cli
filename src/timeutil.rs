use crate::formatters::Quoted;
use crate::identity;
use jiff::civil::{Date, DateTime};
use jiff::fmt::strtime;
use jiff::tz::TimeZone;
use jiff::{Timestamp, ZonedDateTime};
use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

static REL_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^\s*(\d+)\s*(m|min|h|d|w)\s*$").unwrap());
static EXPIRY_REL_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^\s*(\d+)\s*(m|min|h|d|w|y)\s*$").unwrap());
static DATE_ONLY_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d{4}-\d{2}-\d{2}$").unwrap());
static ISO_FRAC_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\.\d{6})\d+").unwrap());
static NUM_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d+(\.\d+)?$").unwrap());

pub fn local_tz() -> TimeZone {
    if let Ok(tz) = std::env::var("TZ") {
        let trimmed = tz.trim().trim_start_matches(':');
        if trimmed == "UTC" || trimmed == "GMT" || trimmed == "Etc/UTC" {
            return TimeZone::UTC;
        }
        if !trimmed.is_empty() {
            if let Ok(z) = TimeZone::get(trimmed) {
                return z;
            }
            if let Ok(z) = TimeZone::posix(trimmed) {
                return z;
            }
        }
    }
    TimeZone::system()
}

pub fn local_now() -> ZonedDateTime {
    Timestamp::now().to_zoned_datetime(local_tz())
}

pub fn now_ms() -> i64 {
    Timestamp::now().as_millisecond()
}

pub fn parse_when(value: Option<&str>, end: bool) -> Result<Option<i64>, String> {
    parse_when_at(value, end, &local_now())
}

pub fn parse_expiry(value: Option<&str>) -> Result<Option<i64>, String> {
    parse_expiry_at(value, &local_now())
}

pub fn parse_expiry_at(value: Option<&str>, now: &ZonedDateTime) -> Result<Option<i64>, String> {
    const MAX_EXPIRY_MS: i64 = 100 * 365 * 86_400 * 1000;
    let Some(raw) = value else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("never") {
        return Ok(None);
    }
    let now_ms = now.timestamp().as_millisecond();
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

pub fn parse_when_at(value: Option<&str>, end: bool, now: &ZonedDateTime) -> Result<Option<i64>, String> {
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
            .timestamp()
            .as_millisecond()
            .checked_sub(delta_ms)
            .ok_or_else(|| bad_time_msg(raw))?;
        return Ok(Some(ts));
    }
    if v == "today" || v == "yesterday" {
        let mut day = now.date();
        if v == "yesterday" {
            day = day.yesterday().map_err(|_| bad_time_msg(raw))?;
        }
        if end {
            day = day.tomorrow().map_err(|_| bad_time_msg(raw))?;
        }
        let ms = first_valid_local_ms(now.time_zone(), day).ok_or_else(|| bad_time_msg(raw))?;
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
        if let Ok(d) = trimmed.parse::<Date>() {
            let day = if end { d.tomorrow().ok() } else { Some(d) };
            if let Some(ms) = day.and_then(|day| first_valid_local_ms(now.time_zone(), day)) {
                return Ok(Some(ms));
            }
        }
    }
    for fmt in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(ndt) = strtime::parse(fmt, trimmed).and_then(|tm| tm.to_datetime()) {
            if let Ok(z) = now.time_zone().to_ambiguous_zoned_datetime(ndt).compatible() {
                return Ok(Some(z.timestamp().as_millisecond()));
            }
        }
    }
    Err(bad_time_msg(raw))
}

pub fn local_midnight_ms(day: Date) -> Option<i64> {
    first_valid_local_ms(&local_tz(), day)
}

pub fn first_valid_local_ms(tz: &TimeZone, day: Date) -> Option<i64> {
    let midnight = day.at(0, 0, 0, 0);
    tz.to_ambiguous_zoned_datetime(midnight)
        .compatible()
        .ok()
        .map(|z| z.timestamp().as_millisecond())
}

fn bad_time_msg(value: &str) -> String {
    format!(
        "Can't read time {}. Use e.g. 30m, 12h, 7d, 2w, today, yesterday, 2026-11-01 or 2026-11-01T14:00.",
        Quoted(value)
    )
}

fn parse_iso_with_offset(s: &str) -> Option<i64> {
    if let Ok(ts) = s.parse::<Timestamp>() {
        return Some(ts.as_millisecond());
    }
    for fmt in [
        "%Y-%m-%dT%H:%M%:z",
        "%Y-%m-%d %H:%M%:z",
        "%Y-%m-%dT%H:%M:%S%.f%z",
        "%Y-%m-%d %H:%M:%S%.f%z",
        "%Y-%m-%dT%H:%M%z",
        "%Y-%m-%d %H:%M%z",
    ] {
        if let Ok(ts) = strtime::parse(fmt, s).and_then(|tm| tm.to_timestamp()) {
            return Some(ts.as_millisecond());
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
    for fmt in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d",
    ] {
        if fmt == "%Y-%m-%d" {
            if let Ok(d) = s.parse::<Date>() {
                if let Ok(z) = d.at(0, 0, 0, 0).to_zoned_datetime(TimeZone::UTC) {
                    return Some(z.timestamp().as_millisecond());
                }
            }
        } else if let Ok(ndt) = strtime::parse(fmt, &s).and_then(|tm| tm.to_datetime()) {
            if let Ok(z) = ndt.to_zoned_datetime(TimeZone::UTC) {
                return Some(z.timestamp().as_millisecond());
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

fn to_local_zoned(ms_val: Option<i64>) -> Option<ZonedDateTime> {
    let ms = ms_val.filter(|&m| m != 0)?;
    let ts = Timestamp::from_millisecond(ms).ok()?;
    Some(ts.to_zoned_datetime(local_tz()))
}

pub fn iso(ms_val: Option<i64>) -> Option<String> {
    let z = to_local_zoned(ms_val)?;
    Some(z.strftime("%Y-%m-%dT%H:%M:%S%:z").to_string())
}

pub fn short(ms_val: Option<i64>) -> String {
    match to_local_zoned(ms_val) {
        Some(z) => z.strftime("%Y-%m-%d %H:%M").to_string(),
        None => "????-??-?? ??:??".to_string(),
    }
}

pub fn day(ms_val: Option<i64>) -> String {
    match to_local_zoned(ms_val) {
        Some(z) => z.strftime("%Y-%m-%d").to_string(),
        None => "Unknown date".to_string(),
    }
}

pub fn hhmm(ms_val: Option<i64>) -> String {
    match to_local_zoned(ms_val) {
        Some(z) => z.strftime("%H:%M").to_string(),
        None => "??:??".to_string(),
    }
}

pub fn parse_datetime_fmt(s: &str, fmt: &str) -> Option<DateTime> {
    strtime::parse(fmt, s).ok().and_then(|tm| tm.to_datetime().ok())
}
