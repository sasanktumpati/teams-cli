use crate::live::error::LiveError;
use crate::live::session::TeamsBearerToken;
use crate::live::transport::Transport;
use serde_json::{json, Value};

fn base(transport: &Transport) -> &str {
    transport.endpoints.presence_base.trim_end_matches('/')
}

fn bearer_header(tok: &TeamsBearerToken) -> String {
    format!("Bearer {}", tok.token)
}

pub fn get_my_presence(transport: &Transport, tok: &TeamsBearerToken) -> Result<Value, LiveError> {
    let url = format!("{}/v1/me/presence", base(transport));
    let auth = bearer_header(tok);
    let resp = transport.request_json("GET", &url, &[("Authorization", &auth)], None, &[&tok.token])?;
    resp.json()
        .ok_or_else(|| LiveError::upstream("Presence service returned non-JSON response."))
}

pub fn get_users_presence(transport: &Transport, tok: &TeamsBearerToken, mris: &[String]) -> Result<Value, LiveError> {
    let url = format!("{}/v1/presence/getpresence/", base(transport));
    let auth = bearer_header(tok);
    let payload: Value = Value::Array(mris.iter().map(|m| json!({ "mri": m })).collect());
    let resp = transport.request_json("POST", &url, &[("Authorization", &auth)], Some(&payload), &[&tok.token])?;
    resp.json()
        .ok_or_else(|| LiveError::upstream("Presence lookup returned non-JSON response."))
}

pub fn set_availability(
    transport: &Transport,
    tok: &TeamsBearerToken,
    availability: &str,
    activity: &str,
    expiry_iso: Option<&str>,
) -> Result<u16, LiveError> {
    let url = format!("{}/v1/me/forceavailability/", base(transport));
    let auth = bearer_header(tok);
    let mut payload = json!({
        "availability": availability,
    });
    if availability == "Offline" && !activity.is_empty() {
        payload["activity"] = Value::String(activity.to_string());
    }
    if let Some(exp) = expiry_iso {
        payload["desiredExpirationTime"] = Value::String(exp.to_string());
    }
    let resp = transport.request_json("PUT", &url, &[("Authorization", &auth)], Some(&payload), &[&tok.token])?;
    Ok(resp.status)
}

pub fn reset_availability(transport: &Transport, tok: &TeamsBearerToken) -> Result<u16, LiveError> {
    let url = format!("{}/v1/me/forceavailability/", base(transport));
    let auth = bearer_header(tok);
    let resp = transport.request_bytes(
        "PUT",
        &url,
        &[("Authorization", &auth), ("Content-Type", "application/json")],
        Some(b""),
        &[&tok.token],
        crate::live::transport::WRITE_TIMEOUT,
    )?;
    Ok(resp.status)
}

pub fn publish_status_message(
    transport: &Transport,
    tok: &TeamsBearerToken,
    message: &str,
    expiry_iso: Option<&str>,
) -> Result<u16, LiveError> {
    let url = format!("{}/v1/me/publishnote", base(transport));
    let auth = bearer_header(tok);
    let expiry = expiry_iso.unwrap_or("9999-12-30T23:59:59.000Z");
    let payload = json!({
        "message": message,
        "expiry": expiry,
    });
    let resp = transport.request_json("PUT", &url, &[("Authorization", &auth)], Some(&payload), &[&tok.token])?;
    Ok(resp.status)
}
