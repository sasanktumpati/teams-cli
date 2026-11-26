use crate::query::resolve::ResolvedMessage;

pub fn percent_encode_conv(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let safe = b.is_ascii_alphanumeric() || matches!(b, b':' | b'@' | b'_' | b'-' | b'.' | b'~');
        if safe {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub fn percent_encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let safe = b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'~');
        if safe {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[derive(Debug, Clone)]
pub struct MessageRef {
    pub conversation_id: String,
    pub chat_name: String,
    pub chat_kind: String,
    pub message_id: String,
    pub thread_root: Option<String>,
    pub sender_mri: String,
}

impl MessageRef {
    pub fn from_resolved(m: &ResolvedMessage) -> Self {
        Self {
            conversation_id: m.conversation_id.clone(),
            chat_name: m.chat_name.clone(),
            chat_kind: m.chat_kind.clone(),
            message_id: m.message_id.clone(),
            thread_root: m.thread_root.clone(),
            sender_mri: m.sender_mri.clone(),
        }
    }

    pub fn conversation_segment(&self) -> String {
        let mut seg = percent_encode_conv(&self.conversation_id);
        if let Some(ref root) = self.thread_root {
            if !root.is_empty() && root != &self.message_id {
                seg.push_str(";messageid=");
                seg.push_str(&percent_encode_segment(root));
            }
        }
        seg
    }
}

#[derive(Debug, Clone)]
pub struct UploadedImage {
    pub object_id: String,
    pub imgo_url: String,
    pub filename: String,
    pub content_type: String,
    pub size_bytes: usize,
}

#[derive(Debug, Clone)]
pub struct UploadedFile {
    pub item_id: String,
    pub file_name: String,
    pub file_type: String,
    pub base_url: String,
    pub object_url: String,
    pub site_url: String,
    pub server_relative_url: String,
    pub share_url: Option<String>,
    pub share_id: Option<String>,
    pub is_channel: bool,
    pub size_bytes: usize,
}
