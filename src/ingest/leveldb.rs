use std::fs;
use std::path::{Path, PathBuf};

const LDB_FOOTER_SIZE: usize = 48;
const LDB_BLOCK_TRAILER_SIZE: usize = 5;
const LDB_MAGIC: u64 = 0xdb47_7524_8b80_fb57;

const LOG_BLOCK_SIZE: usize = 32_768;
const LOG_HEADER_SIZE: usize = 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyState {
    Deleted,
    Live,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct Record {
    pub user_key: Vec<u8>,
    pub value: Vec<u8>,
    pub seq: u64,
    pub state: KeyState,
}

pub fn read_varint(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let mut result = 0u64;
    let mut shift = 0u32;
    let mut count = 0usize;
    while *pos < buf.len() && count < 10 {
        let b = buf[*pos];
        *pos += 1;
        result |= ((b & 0x7f) as u64) << shift;
        if (b & 0x80) == 0 {
            return Some(result);
        }
        shift += 7;
        count += 1;
    }
    None
}

fn read_block_handle(buf: &[u8], pos: &mut usize) -> Option<(u64, u64)> {
    let offset = read_varint(buf, pos)?;
    let length = read_varint(buf, pos)?;
    Some((offset, length))
}

fn read_ldb_block(decoder: &mut snap::raw::Decoder, file_data: &[u8], offset: u64, length: u64) -> Option<Vec<u8>> {
    let start = usize::try_from(offset).ok()?;
    let len = usize::try_from(length).ok()?;
    let end = start.checked_add(len)?;
    let trailer_end = end.checked_add(LDB_BLOCK_TRAILER_SIZE)?;
    if trailer_end > file_data.len() {
        return None;
    }
    let raw = &file_data[start..end];
    let comp_type = file_data[end];
    if comp_type == 0 {
        Some(raw.to_vec())
    } else if comp_type == 1 {
        decoder.decompress_vec(raw).ok()
    } else {
        None
    }
}

fn iter_block_entries(block: &[u8], mut cb: impl FnMut(&[u8], &[u8])) {
    if block.len() < 4 {
        return;
    }
    let restart_count = u32::from_le_bytes(block[block.len() - 4..].try_into().unwrap()) as usize;
    let Some(restart_bytes) = restart_count.checked_add(1).and_then(|n| n.checked_mul(4)) else {
        return;
    };
    if restart_bytes > block.len() {
        return;
    }
    let restart_offset = block.len() - restart_bytes;
    let first_entry_off = if restart_count > 0 && restart_offset + 4 <= block.len() {
        u32::from_le_bytes(block[restart_offset..restart_offset + 4].try_into().unwrap()) as usize
    } else {
        0
    };
    if first_entry_off > restart_offset {
        return;
    }
    let mut pos = first_entry_off;
    let mut key: Vec<u8> = Vec::with_capacity(128);
    while pos < restart_offset {
        let Some(shared) = read_varint(block, &mut pos).map(|v| v as usize) else {
            break;
        };
        let Some(non_shared) = read_varint(block, &mut pos).map(|v| v as usize) else {
            break;
        };
        let Some(val_len) = read_varint(block, &mut pos).map(|v| v as usize) else {
            break;
        };
        let Some(key_end) = pos.checked_add(non_shared) else {
            break;
        };
        let Some(val_end) = key_end.checked_add(val_len) else {
            break;
        };
        if shared > key.len() || val_end > restart_offset {
            break;
        }
        key.truncate(shared);
        key.extend_from_slice(&block[pos..key_end]);
        let val = &block[key_end..val_end];
        pos = val_end;
        cb(&key, val);
    }
}

fn scan_ldb_file(path: &Path, decoder: &mut snap::raw::Decoder, cb: &mut impl FnMut(&[u8], &[u8], u64, KeyState)) {
    let Ok(data) = fs::read(path) else {
        return;
    };
    if data.len() < LDB_FOOTER_SIZE {
        return;
    }
    let footer = &data[data.len() - LDB_FOOTER_SIZE..];
    let magic = u64::from_le_bytes(footer[LDB_FOOTER_SIZE - 8..].try_into().unwrap());
    if magic != LDB_MAGIC {
        return;
    }
    let mut fpos = 0usize;
    let Some(_meta_handle) = read_block_handle(footer, &mut fpos) else {
        return;
    };
    let Some((idx_off, idx_len)) = read_block_handle(footer, &mut fpos) else {
        return;
    };
    let Some(idx_block) = read_ldb_block(decoder, &data, idx_off, idx_len) else {
        return;
    };
    let mut handles: Vec<(u64, u64)> = Vec::new();
    iter_block_entries(&idx_block, |_k, v| {
        let mut p = 0;
        if let Some(h) = read_block_handle(v, &mut p) {
            handles.push(h);
        }
    });
    for (off, len) in handles {
        let Some(block) = read_ldb_block(decoder, &data, off, len) else {
            continue;
        };
        iter_block_entries(&block, |raw_key, val| {
            let (user_key, seq, state) = if raw_key.len() >= 8 {
                let tag = u64::from_le_bytes(raw_key[raw_key.len() - 8..].try_into().unwrap());
                let seq = tag >> 8;
                let state = if raw_key.len() > 8 {
                    if raw_key[raw_key.len() - 8] == 0 {
                        KeyState::Deleted
                    } else {
                        KeyState::Live
                    }
                } else {
                    KeyState::Unknown
                };
                (&raw_key[..raw_key.len() - 8], seq, state)
            } else {
                (raw_key, 0, KeyState::Unknown)
            };
            cb(user_key, val, seq, state);
        });
    }
}

fn scan_log_file(path: &Path, cb: &mut impl FnMut(&[u8], &[u8], u64, KeyState)) {
    let Ok(data) = fs::read(path) else {
        return;
    };
    let mut in_record = false;
    let mut fragments: Vec<u8> = Vec::new();
    let mut batches: Vec<Vec<u8>> = Vec::new();

    for block in data.chunks(LOG_BLOCK_SIZE) {
        let mut pos = 0usize;
        while block.len().saturating_sub(pos) >= LOG_HEADER_SIZE {
            let _crc = u32::from_le_bytes(block[pos..pos + 4].try_into().unwrap());
            let length = u16::from_le_bytes(block[pos + 4..pos + 6].try_into().unwrap()) as usize;
            let rec_type = block[pos + 6];
            let data_start = pos + LOG_HEADER_SIZE;
            if data_start + length > block.len() {
                in_record = false;
                fragments.clear();
                break;
            }
            if rec_type == 0 && length == 0 {
                in_record = false;
                fragments.clear();
                break;
            }
            let payload = &block[data_start..data_start + length];
            pos = data_start + length;
            match rec_type {
                1 => {
                    in_record = false;
                    fragments.clear();
                    batches.push(payload.to_vec());
                }
                2 => {
                    in_record = true;
                    fragments.clear();
                    fragments.extend_from_slice(payload);
                }
                3 => {
                    if in_record {
                        fragments.extend_from_slice(payload);
                    }
                }
                4 => {
                    if in_record {
                        fragments.extend_from_slice(payload);
                        batches.push(std::mem::take(&mut fragments));
                        in_record = false;
                    }
                }
                _ => {
                    in_record = false;
                    fragments.clear();
                }
            }
        }
    }

    for batch in batches {
        if batch.len() < 12 {
            continue;
        }
        let seq = u64::from_le_bytes(batch[0..8].try_into().unwrap());
        let count = u32::from_le_bytes(batch[8..12].try_into().unwrap());
        let mut pos = 12usize;
        for i in 0..count {
            if pos >= batch.len() {
                break;
            }
            let state = match batch[pos] {
                0 => KeyState::Deleted,
                1 => KeyState::Live,
                _ => break,
            };
            pos += 1;
            let Some(klen) = read_varint(&batch, &mut pos).and_then(|v| usize::try_from(v).ok()) else {
                break;
            };
            let Some(kend) = pos.checked_add(klen) else {
                break;
            };
            if kend > batch.len() {
                break;
            }
            let key = &batch[pos..kend];
            pos = kend;
            let val: &[u8] = if state != KeyState::Deleted {
                let Some(vlen) = read_varint(&batch, &mut pos).and_then(|v| usize::try_from(v).ok()) else {
                    break;
                };
                let Some(vend) = pos.checked_add(vlen) else {
                    break;
                };
                if vend > batch.len() {
                    break;
                }
                let v = &batch[pos..vend];
                pos = vend;
                v
            } else {
                &[]
            };
            cb(key, val, seq.saturating_add(i as u64), state);
        }
    }
}

pub fn for_each_record(dir: &Path, mut cb: impl FnMut(&[u8], &[u8], u64, KeyState)) -> Result<(), String> {
    if !dir.is_dir() {
        return Err(format!("LevelDB directory not found: {}", dir.display()));
    }
    let mut files: Vec<(u64, bool, PathBuf)> = Vec::new();
    let rd = fs::read_dir(dir).map_err(|e| format!("read_dir {}: {e}", dir.display()))?;
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some((stem, ext)) = name.rsplit_once('.') else {
            continue;
        };
        if stem.len() < 6 || !stem.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let is_log = match ext.to_ascii_lowercase().as_str() {
            "log" => true,
            "ldb" | "sst" => false,
            _ => continue,
        };
        if let Ok(file_no) = stem.parse::<u64>() {
            files.push((file_no, is_log, path));
        }
    }
    files.sort_by_key(|(no, _, _)| *no);

    let mut decoder = snap::raw::Decoder::new();
    for (_, is_log, path) in files {
        if is_log {
            scan_log_file(&path, &mut cb);
        } else {
            scan_ldb_file(&path, &mut decoder, &mut cb);
        }
    }
    Ok(())
}

pub fn read_all_records(dir: &Path) -> Result<Vec<Record>, String> {
    let mut out = Vec::with_capacity(65_536);
    for_each_record(dir, |user_key, value, seq, state| {
        out.push(Record {
            user_key: user_key.to_vec(),
            value: value.to_vec(),
            seq,
            state,
        });
    })?;
    out.sort_by_key(|r| r.seq);
    Ok(out)
}
