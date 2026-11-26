use crate::live::error::LiveError;
use crate::live::refs::{percent_encode_segment, UploadedImage};
use crate::live::session::SkypeToken;
use crate::live::transport::{Transport, WRITE_TIMEOUT};
use serde_json::json;

pub const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

pub fn upload_image(
    transport: &Transport,
    tok: &SkypeToken,
    conv_id: &str,
    filename: &str,
    content_type: &str,
    bytes: &[u8],
) -> Result<UploadedImage, LiveError> {
    if bytes.is_empty() {
        return Err(LiveError::bad(format!("Image '{filename}' is empty.")));
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(LiveError::bad(format!(
            "Image '{filename}' is {} bytes (max {} bytes).",
            bytes.len(),
            MAX_IMAGE_BYTES
        )));
    }
    let base = transport.endpoints.ams_base.trim_end_matches('/');
    let create_url = format!("{base}/v1/objects");
    let auth = format!("skype_token {}", tok.token);
    let create_payload = json!({
        "type": "pish/image",
        "permissions": {
            conv_id: ["read"]
        },
        "filename": filename,
        "sharingMode": "Inline",
    });
    let resp = transport.request_json(
        "POST",
        &create_url,
        &[("Authorization", &auth), ("X-Client-Version", "teams-cli")],
        Some(&create_payload),
        &[&tok.token],
    )?;
    let object_id = resp
        .json()
        .and_then(|v| v.get("id").and_then(|s| s.as_str()).map(String::from))
        .filter(|s| !s.is_empty())
        .ok_or_else(|| LiveError::upstream("AMS did not return an object id for the image upload."))?;

    let enc_id = percent_encode_segment(&object_id);
    let upload_url = format!("{base}/v1/objects/{enc_id}/content/imgpsh");
    let _ = transport.request_bytes(
        "PUT",
        &upload_url,
        &[("Authorization", &auth), ("Content-Type", content_type)],
        Some(bytes),
        &[&tok.token],
        WRITE_TIMEOUT,
    )?;

    let imgo_url = format!("{base}/v1/objects/{enc_id}/views/imgo");
    Ok(UploadedImage {
        object_id,
        imgo_url,
        filename: filename.to_string(),
        content_type: content_type.to_string(),
        size_bytes: bytes.len(),
    })
}
