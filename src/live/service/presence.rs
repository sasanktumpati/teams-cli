use crate::live::clients::presence as presence_client;
use crate::live::error::LiveError;
use crate::live::policy::{self, Action, Principal};
use crate::live::service::{resolve_scope, ArchiveSource, LiveService};
use crate::query::resolve::{self, Strictness};
use crate::timeutil;
use jiff::Timestamp;
use serde_json::{json, Value};

fn normalize_availability(raw: &str) -> Result<Option<(&'static str, &'static str)>, LiveError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "available" | "online" => Ok(Some(("Available", "Available"))),
        "busy" => Ok(Some(("Busy", "Busy"))),
        "dnd" | "donotdisturb" | "do-not-disturb" => Ok(Some(("DoNotDisturb", "DoNotDisturb"))),
        "brb" | "berightback" | "be-right-back" => Ok(Some(("BeRightBack", "BeRightBack"))),
        "away" => Ok(Some(("Away", "Away"))),
        "offline" | "appearoffline" | "appear-offline" => Ok(Some(("Offline", "OffWork"))),
        "reset" => Ok(None),
        other => Err(LiveError::bad(format!(
            "Invalid availability '{other}'. Use one of: available, busy, dnd, brb, away, offline, reset."
        ))),
    }
}

pub const MAX_PRESENCE_PEOPLE: usize = 50;

fn parse_expiry_iso(raw: Option<&str>) -> Result<Option<String>, LiveError> {
    let Some(spec) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let Some(ms) = timeutil::parse_expiry(Some(spec)).map_err(LiveError::bad)? else {
        return Ok(None);
    };
    let ts = Timestamp::from_millisecond(ms).map_err(|e| LiveError::bad(e.to_string()))?;
    Ok(Some(ts.strftime("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()))
}

impl LiveService {
    pub fn get_presence(
        &self,
        archive: ArchiveSource<'_>,
        principal: &Principal,
        people: &[String],
    ) -> Result<Value, LiveError> {
        policy::authorize(principal, Action::GetPresence, None, self.daemon_writes_enabled)?;
        if people.len() > MAX_PRESENCE_PEOPLE {
            return Err(LiveError::bad(format!(
                "Too many people requested ({}; max {MAX_PRESENCE_PEOPLE}).",
                people.len()
            )));
        }

        if people.is_empty() {
            let tok = self.session.bearer_token()?;
            let raw = self.track_upstream(presence_client::get_my_presence(&self.transport, &tok))?;
            return Ok(json!({
                "ok": true,
                "self": true,
                "availability": raw.get("availability").and_then(Value::as_str).unwrap_or("Unknown"),
                "activity": raw.get("activity").and_then(Value::as_str).unwrap_or("Unknown"),
                "lastActiveTime": raw.get("lastActiveTime"),
                "deviceType": raw.get("deviceType"),
                "raw": raw,
            }));
        }

        let scope = resolve_scope(principal);
        let resolved = archive.with_conn(|conn| {
            let mut list = Vec::with_capacity(people.len());
            for q in people {
                let (p, _) = resolve::resolve_person_scoped(conn, q, Strictness::Strict, scope)?;
                list.push(p);
            }
            Ok(list)
        })?;
        let tok = self.session.bearer_token()?;
        let mris: Vec<String> = resolved.iter().map(|p| p.mri.clone()).collect();
        let raw = self.track_upstream(presence_client::get_users_presence(&self.transport, &tok, &mris))?;
        let items: Vec<Value> = raw
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|entry| {
                let mri = entry.get("mri").and_then(Value::as_str).unwrap_or("");
                let name = resolved
                    .iter()
                    .find(|p| crate::identity::same_person(Some(&p.mri), Some(mri)))
                    .map(|p| p.name.as_str())
                    .unwrap_or(mri);
                let pres = entry.get("presence").unwrap_or(&entry);
                json!({
                    "mri": mri,
                    "name": name,
                    "availability": pres.get("availability").and_then(Value::as_str).unwrap_or("Unknown"),
                    "activity": pres.get("activity").and_then(Value::as_str).unwrap_or("Unknown"),
                    "statusMessage": pres.get("statusMessage"),
                })
            })
            .collect();

        Ok(json!({
            "ok": true,
            "self": false,
            "presence": items,
        }))
    }

    pub fn set_presence(
        &self,
        principal: &Principal,
        availability: &str,
        expires: Option<&str>,
        dry_run: bool,
    ) -> Result<Value, LiveError> {
        policy::authorize_principal(principal, Action::SetPresence, None)?;
        let mapped = normalize_availability(availability)?;
        let expiry_iso = parse_expiry_iso(expires)?;
        let auth_check = policy::authorize(principal, Action::SetPresence, None, self.daemon_writes_enabled);

        if dry_run {
            return Ok(json!({
                "ok": true,
                "dryRun": true,
                "applied": false,
                "availability": mapped.map(|(a, _)| a).unwrap_or("Reset"),
                "expiresAt": expiry_iso,
            }));
        }

        auth_check?;
        self.check_write_rate_limit(principal, Action::SetPresence)?;

        let tok = self.session.bearer_token()?;
        let status = self.track_upstream(match mapped {
            Some((avail, act)) => {
                presence_client::set_availability(&self.transport, &tok, avail, act, expiry_iso.as_deref())
            }
            None => presence_client::reset_availability(&self.transport, &tok),
        })?;
        policy::append_audit_entry(principal, Action::SetPresence, None, None, None);

        Ok(json!({
            "ok": true,
            "dryRun": false,
            "applied": true,
            "status": status,
            "availability": mapped.map(|(a, _)| a).unwrap_or("Reset"),
            "expiresAt": expiry_iso,
        }))
    }

    pub fn set_status_message(
        &self,
        principal: &Principal,
        message: Option<&str>,
        clear: bool,
        expires: Option<&str>,
        dry_run: bool,
    ) -> Result<Value, LiveError> {
        let text = if clear { "" } else { message.unwrap_or("").trim() };
        if !clear && text.is_empty() {
            return Err(LiveError::bad(
                "Status message text cannot be empty (use --clear / clear: true to clear).",
            ));
        }
        let expiry_iso = if clear {
            Some("1970-01-01T00:00:00.000Z".to_string())
        } else {
            parse_expiry_iso(expires)?
        };
        let action = if clear {
            Action::ClearStatusMessage
        } else {
            Action::SetStatusMessage
        };
        policy::authorize_principal(principal, action, None)?;
        let auth_check = policy::authorize(principal, action, None, self.daemon_writes_enabled);

        if dry_run {
            return Ok(json!({
                "ok": true,
                "dryRun": true,
                "applied": false,
                "cleared": clear,
                "message": text,
                "expiresAt": expiry_iso,
            }));
        }

        auth_check?;
        self.check_write_rate_limit(principal, action)?;

        let tok = self.session.bearer_token()?;
        let status = self.track_upstream(presence_client::publish_status_message(
            &self.transport,
            &tok,
            text,
            expiry_iso.as_deref(),
        ))?;
        let hash = (!clear).then(|| policy::content_sha256_hex(text));
        policy::append_audit_entry(principal, action, None, None, hash.as_deref());

        Ok(json!({
            "ok": true,
            "dryRun": false,
            "applied": true,
            "status": status,
            "cleared": clear,
            "message": text,
            "expiresAt": expiry_iso,
        }))
    }
}
