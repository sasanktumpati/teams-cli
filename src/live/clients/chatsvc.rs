use crate::live::error::LiveError;
use crate::live::refs::{percent_encode_conv, percent_encode_segment, MessageRef};
use crate::live::session::SkypeToken;
use crate::live::transport::Transport;
use crate::timeutil;
use serde_json::{json, Value};

fn chatsvc_base(transport: &Transport, region: &str) -> String {
    format!(
        "{}/api/chatsvc/{region}/v1",
        transport.endpoints.chatsvc_base.trim_end_matches('/')
    )
}

fn auth_header(tok: &SkypeToken) -> String {
    format!("skypetoken={}", tok.token)
}

pub fn send_message(
    transport: &Transport,
    tok: &SkypeToken,
    conv_id: &str,
    reply_to: Option<&str>,
    payload: &Value,
) -> Result<(u16, Option<String>), LiveError> {
    let mut conv_seg = percent_encode_conv(conv_id);
    if let Some(rt) = reply_to.filter(|s| !s.is_empty()) {
        conv_seg.push_str(";messageid=");
        conv_seg.push_str(&percent_encode_segment(rt));
    }
    let url = format!(
        "{}/users/ME/conversations/{conv_seg}/messages",
        chatsvc_base(transport, &tok.region)
    );
    let auth = auth_header(tok);
    let resp = transport.request_json("POST", &url, &[("Authentication", &auth)], Some(payload), &[&tok.token])?;
    let msg_id = resp.json().and_then(|v| {
        let oat = v.get("OriginalArrivalTime").or_else(|| v.get("originalArrivalTime"))?;
        if oat.is_boolean() || oat.is_null() {
            None
        } else if let Some(s) = oat.as_str() {
            Some(s.to_string())
        } else {
            Some(oat.to_string())
        }
    });
    Ok((resp.status, msg_id))
}

pub fn edit_message(
    transport: &Transport,
    tok: &SkypeToken,
    target: &MessageRef,
    content_html: &str,
    extra_properties: Option<&Value>,
) -> Result<u16, LiveError> {
    let conv_seg = target.conversation_segment();
    let msg_seg = percent_encode_segment(&target.message_id);
    let url = format!(
        "{}/users/ME/conversations/{conv_seg}/messages/{msg_seg}",
        chatsvc_base(transport, &tok.region)
    );
    let mut payload = json!({
        "content": content_html,
        "messagetype": "RichText/Html",
        "contenttype": "text",
        "edittime": timeutil::now_ms().to_string(),
    });
    if let Some(props) = extra_properties {
        payload["properties"] = props.clone();
    }
    let auth = auth_header(tok);
    let resp = transport.request_json("PUT", &url, &[("Authentication", &auth)], Some(&payload), &[&tok.token])?;
    Ok(resp.status)
}

pub fn delete_message(transport: &Transport, tok: &SkypeToken, target: &MessageRef) -> Result<u16, LiveError> {
    let conv_seg = target.conversation_segment();
    let msg_seg = percent_encode_segment(&target.message_id);
    let url = format!(
        "{}/users/ME/conversations/{conv_seg}/messages/{msg_seg}",
        chatsvc_base(transport, &tok.region)
    );
    let auth = auth_header(tok);
    let resp = transport.request_json("DELETE", &url, &[("Authentication", &auth)], None, &[&tok.token])?;
    Ok(resp.status)
}

pub fn set_reaction(
    transport: &Transport,
    tok: &SkypeToken,
    target: &MessageRef,
    emotion_key: &str,
    add: bool,
) -> Result<u16, LiveError> {
    let conv_seg = target.conversation_segment();
    let msg_seg = percent_encode_segment(&target.message_id);
    let url = format!(
        "{}/users/ME/conversations/{conv_seg}/messages/{msg_seg}/properties?name=emotions",
        chatsvc_base(transport, &tok.region)
    );
    let payload = json!({
        "emotions": {
            "key": emotion_key,
            "value": timeutil::now_ms().to_string(),
        }
    });
    let auth = auth_header(tok);
    let method = if add { "PUT" } else { "DELETE" };
    let resp = transport.request_json(
        method,
        &url,
        &[("Authentication", &auth)],
        Some(&payload),
        &[&tok.token],
    )?;
    Ok(resp.status)
}

pub fn forward_message(
    transport: &Transport,
    tok: &SkypeToken,
    source_conv_id: &str,
    source_message_id: &str,
    dest_conv_id: &str,
    additional_message: &Value,
) -> Result<(u16, Option<String>), LiveError> {
    let conv_seg = percent_encode_conv(source_conv_id);
    let url = format!(
        "{}/users/ME/conversations/{conv_seg}/messages/forward",
        chatsvc_base(transport, &tok.region)
    );
    let payload = json!({
        "targetThreadIds": [dest_conv_id],
        "messageIds": [source_message_id],
        "additionalMessage": additional_message,
        "templateId": "basic_forward_message_template",
    });
    let auth = auth_header(tok);
    let resp = transport.request_json(
        "POST",
        &url,
        &[("Authentication", &auth)],
        Some(&payload),
        &[&tok.token],
    )?;
    let parsed = resp.json();
    if let Some(arr) = parsed.as_ref().and_then(Value::as_array) {
        if let Some(first) = arr.first() {
            let inner_status = first.get("statusCode").and_then(Value::as_u64).unwrap_or(200);
            if !(200..=299).contains(&inner_status) || first.get("error").is_some_and(|e| !e.is_null()) {
                let err_detail = first
                    .get("error")
                    .map(crate::json_codec::to_string)
                    .unwrap_or_else(|| format!("HTTP {inner_status}"));
                return Err(LiveError::upstream(format!(
                    "Teams rejected forwarded message: {err_detail}"
                )));
            }
            let msg_id = first
                .get("messageId")
                .or_else(|| first.get("OriginalArrivalTime"))
                .or_else(|| first.get("originalArrivalTime"))
                .and_then(|v| {
                    if v.is_null() || v.is_boolean() {
                        None
                    } else if let Some(s) = v.as_str() {
                        Some(s.to_string())
                    } else {
                        Some(v.to_string())
                    }
                });
            return Ok((inner_status as u16, msg_id));
        }
    }
    let msg_id = parsed.and_then(|v| {
        let oat = v
            .get("messageId")
            .or_else(|| v.get("OriginalArrivalTime"))
            .or_else(|| v.get("originalArrivalTime"))?;
        if oat.is_boolean() || oat.is_null() {
            None
        } else if let Some(s) = oat.as_str() {
            Some(s.to_string())
        } else {
            Some(oat.to_string())
        }
    });
    Ok((resp.status, msg_id))
}

pub fn set_pinned(transport: &Transport, tok: &SkypeToken, target: &MessageRef, pin: bool) -> Result<u16, LiveError> {
    let conv_seg = target.conversation_segment();
    let msg_seg = percent_encode_segment(&target.message_id);
    let url = format!(
        "{}/users/ME/conversations/{conv_seg}/messages/{msg_seg}/properties?name=pinned",
        chatsvc_base(transport, &tok.region)
    );
    let auth = auth_header(tok);
    let (method, body) = if pin {
        ("PUT", json!({ "pinned": true }))
    } else {
        ("DELETE", json!({ "pinned": Value::Null }))
    };
    let resp = transport.request_json(method, &url, &[("Authentication", &auth)], Some(&body), &[&tok.token])?;
    Ok(resp.status)
}

pub fn set_saved(transport: &Transport, tok: &SkypeToken, target: &MessageRef, save: bool) -> Result<u16, LiveError> {
    let conv_seg = percent_encode_conv(&target.conversation_id);
    let root_id = target.thread_root.as_deref().unwrap_or(&target.message_id);
    let root_seg = percent_encode_segment(root_id);
    let url = format!(
        "{}/users/ME/conversations/{conv_seg}/rcmetadata/{root_seg}",
        chatsvc_base(transport, &tok.region)
    );
    let mid_val: Value = target
        .message_id
        .parse::<i64>()
        .map(Value::from)
        .unwrap_or_else(|_| Value::String(target.message_id.clone()));
    let body = json!({
        "s": if save { 1 } else { 0 },
        "mid": mid_val,
    });
    let auth = auth_header(tok);
    let resp = transport.request_json("PUT", &url, &[("Authentication", &auth)], Some(&body), &[&tok.token])?;
    Ok(resp.status)
}

pub fn set_consumption_horizon(
    transport: &Transport,
    tok: &SkypeToken,
    conv_id: &str,
    horizon: &str,
) -> Result<u16, LiveError> {
    let conv_seg = percent_encode_conv(conv_id);
    let url = format!(
        "{}/users/ME/conversations/{conv_seg}/properties?name=consumptionhorizon",
        chatsvc_base(transport, &tok.region)
    );
    let payload = json!({ "consumptionhorizon": horizon });
    let auth = auth_header(tok);
    let resp = transport.request_json("PUT", &url, &[("Authentication", &auth)], Some(&payload), &[&tok.token])?;
    Ok(resp.status)
}

pub fn set_alerts_muted(transport: &Transport, tok: &SkypeToken, conv_id: &str, muted: bool) -> Result<u16, LiveError> {
    let conv_seg = percent_encode_conv(conv_id);
    let url = format!(
        "{}/users/ME/conversations/{conv_seg}/properties?name=alerts",
        chatsvc_base(transport, &tok.region)
    );
    let payload = json!({ "alerts": (!muted).to_string() });
    let auth = auth_header(tok);
    let resp = transport.request_json("PUT", &url, &[("Authentication", &auth)], Some(&payload), &[&tok.token])?;
    Ok(resp.status)
}

pub fn create_thread(
    transport: &Transport,
    tok: &SkypeToken,
    member_mris: &[String],
    topic: Option<&str>,
) -> Result<(u16, Option<String>), LiveError> {
    let url = format!("{}/threads", chatsvc_base(transport, &tok.region));
    let members: Vec<Value> = member_mris
        .iter()
        .map(|mri| json!({ "id": mri, "role": "Admin" }))
        .collect();
    let mut props = json!({
        "threadType": "chat",
        "fixedRoster": member_mris.len() <= 2,
    });
    if let Some(t) = topic.map(str::trim).filter(|s| !s.is_empty()) {
        props["topic"] = Value::String(t.to_string());
    }
    let payload = json!({
        "members": members,
        "properties": props,
    });
    let auth = auth_header(tok);
    let resp = transport.request_json(
        "POST",
        &url,
        &[("Authentication", &auth)],
        Some(&payload),
        &[&tok.token],
    )?;
    let thread_id = resp
        .location
        .as_deref()
        .and_then(|loc| {
            let clean = loc.split(['?', '#']).next().unwrap_or(loc);
            clean
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .filter(|s| !s.is_empty())
                .map(crate::live::cookies::urlencoding_decode)
        })
        .or_else(|| {
            resp.json()
                .and_then(|v| v.get("id").and_then(|s| s.as_str()).map(String::from))
        });
    Ok((resp.status, thread_id))
}

pub fn rename_thread(transport: &Transport, tok: &SkypeToken, conv_id: &str, topic: &str) -> Result<u16, LiveError> {
    let conv_seg = percent_encode_conv(conv_id);
    let url = format!(
        "{}/threads/{conv_seg}/properties?name=topic",
        chatsvc_base(transport, &tok.region)
    );
    let payload = json!({ "topic": topic });
    let auth = auth_header(tok);
    let resp = transport.request_json("PUT", &url, &[("Authentication", &auth)], Some(&payload), &[&tok.token])?;
    Ok(resp.status)
}

pub fn add_member(transport: &Transport, tok: &SkypeToken, conv_id: &str, member_mri: &str) -> Result<u16, LiveError> {
    let conv_seg = percent_encode_conv(conv_id);
    let member_seg = percent_encode_conv(member_mri);
    let url = format!(
        "{}/threads/{conv_seg}/members/{member_seg}",
        chatsvc_base(transport, &tok.region)
    );
    let payload = json!({ "role": "Admin" });
    let auth = auth_header(tok);
    let resp = transport.request_json("PUT", &url, &[("Authentication", &auth)], Some(&payload), &[&tok.token])?;
    Ok(resp.status)
}

pub fn remove_member(
    transport: &Transport,
    tok: &SkypeToken,
    conv_id: &str,
    member_mri: &str,
) -> Result<u16, LiveError> {
    let conv_seg = percent_encode_conv(conv_id);
    let member_seg = percent_encode_conv(member_mri);
    let url = format!(
        "{}/threads/{conv_seg}/members/{member_seg}",
        chatsvc_base(transport, &tok.region)
    );
    let auth = auth_header(tok);
    let resp = transport.request_json("DELETE", &url, &[("Authentication", &auth)], None, &[&tok.token])?;
    Ok(resp.status)
}
