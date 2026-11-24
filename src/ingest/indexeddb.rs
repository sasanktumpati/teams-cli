use crate::ingest::leveldb::{self, read_varint, KeyState};
use crate::ingest::v8::Deserializer;
use crate::ingest::SourceError;
use crate::paths;
use regex::Regex;
use serde_json::{Number, Value};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

const BLINK_TRAILER_TAG: u8 = 0xFE;
const BLINK_TRAILER_SIZE: usize = 13;
const BLINK_MIN_TRAILER_VERSION: u64 = 21;
pub const MAX_DEPTH: usize = 64;

static USER_OID_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r":8:orgid:([0-9a-fA-F-]{36}):").unwrap());

#[derive(Debug, Clone)]
pub struct IdbKey {
    pub raw_key: Vec<u8>,
    pub value: Value,
}

impl IdbKey {
    pub fn parse(buf: &[u8]) -> Result<(Self, usize), String> {
        Self::parse_depth(buf, 0)
    }

    fn parse_depth(buf: &[u8], depth: usize) -> Result<(Self, usize), String> {
        if depth >= MAX_DEPTH {
            return Err("IdbKey nesting depth exceeded".to_string());
        }
        if buf.is_empty() {
            return Err("empty IdbKey".to_string());
        }
        let kind = buf[0];
        let rest = &buf[1..];
        let mut pos = 0usize;
        let (value, used_rest) = match kind {
            0 => (Value::Null, 0),
            1 => {
                let char_len = read_varint(rest, &mut pos)
                    .and_then(|v| usize::try_from(v).ok())
                    .ok_or("truncated IdbKey string len")?;
                let byte_len = char_len.checked_mul(2).ok_or("IdbKey string len overflow")?;
                let end = pos.checked_add(byte_len).ok_or("IdbKey string overflow")?;
                if end > rest.len() {
                    return Err("truncated IdbKey string".to_string());
                }
                let s = decode_utf16be(&rest[pos..end]);
                (Value::String(s), end)
            }
            2 | 3 => {
                if rest.len() < 8 {
                    return Err("truncated IdbKey number/date".to_string());
                }
                let f = f64::from_le_bytes(rest[..8].try_into().unwrap());
                let val = Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null);
                (val, 8)
            }
            4 => {
                let count = read_varint(rest, &mut pos)
                    .and_then(|v| usize::try_from(v).ok())
                    .ok_or("truncated IdbKey array count")?;
                let rem = rest.len().saturating_sub(pos);
                if count > rem {
                    return Err("truncated IdbKey array".to_string());
                }
                let mut items = Vec::with_capacity(count.min(1024));
                for _ in 0..count {
                    let (sub, used) = IdbKey::parse_depth(&rest[pos..], depth + 1)?;
                    pos += used;
                    items.push(sub.value);
                }
                (Value::Array(items), pos)
            }
            6 => {
                let len = read_varint(rest, &mut pos)
                    .and_then(|v| usize::try_from(v).ok())
                    .ok_or("truncated IdbKey binary len")?;
                let end = pos.checked_add(len).ok_or("IdbKey binary overflow")?;
                if end > rest.len() {
                    return Err("truncated IdbKey binary".to_string());
                }
                let s: String = rest[pos..end].iter().map(|&b| b as char).collect();
                (Value::String(s), end)
            }
            other => return Err(format!("unsupported IdbKey type {other}")),
        };
        let total = 1 + used_rest;
        Ok((
            IdbKey {
                raw_key: buf[..total].to_vec(),
                value,
            },
            total,
        ))
    }
}

fn decode_utf16be(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_be_bytes(*c))
        .collect();
    String::from_utf16_lossy(&units)
}

fn decode_truncated_int(bytes: &[u8]) -> Option<u64> {
    if bytes.is_empty() || bytes.len() > 8 {
        return None;
    }
    let mut res = 0u64;
    for (i, &b) in bytes.iter().enumerate() {
        res |= (b as u64) << (i * 8);
    }
    Some(res)
}

fn read_prefix(key: &[u8]) -> Option<(u64, u64, u64, usize)> {
    if key.is_empty() {
        return None;
    }
    let lengths = key[0];
    let db_len = (((lengths >> 5) & 0x07) + 1) as usize;
    let store_len = (((lengths >> 2) & 0x07) + 1) as usize;
    let idx_len = ((lengths & 0x03) + 1) as usize;
    let total = 1 + db_len + store_len + idx_len;
    if total > key.len() {
        return None;
    }
    let mut off = 1;
    let db_id = decode_truncated_int(&key[off..off + db_len])?;
    off += db_len;
    let store_id = decode_truncated_int(&key[off..off + store_len])?;
    off += store_len;
    let index_id = decode_truncated_int(&key[off..off + idx_len])?;
    Some((db_id, store_id, index_id, total))
}

#[derive(Debug, Clone)]
struct BlobInfo {
    blob_number: u64,
}

fn parse_blob_infos(val: &[u8]) -> Vec<BlobInfo> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < val.len() {
        let blob_type = val[pos];
        pos += 1;
        if blob_type != 0 && blob_type != 1 {
            break;
        }
        let Some(blob_number) = read_varint(val, &mut pos) else {
            break;
        };
        let Some(mime_bytes) = read_varint(val, &mut pos)
            .and_then(|v| usize::try_from(v).ok())
            .and_then(|n| n.checked_mul(2))
        else {
            break;
        };
        let Some(next_pos) = pos.checked_add(mime_bytes) else {
            break;
        };
        pos = next_pos;
        if pos > val.len() {
            break;
        }
        let Some(_data_size) = read_varint(val, &mut pos) else {
            break;
        };
        if blob_type == 1 {
            let Some(name_bytes) = read_varint(val, &mut pos)
                .and_then(|v| usize::try_from(v).ok())
                .and_then(|n| n.checked_mul(2))
            else {
                break;
            };
            let Some(next_pos) = pos.checked_add(name_bytes) else {
                break;
            };
            pos = next_pos;
            if pos > val.len() {
                break;
            }
            let _ = read_varint(val, &mut pos);
        }
        out.push(BlobInfo { blob_number });
    }
    out
}

type BlobKey = (u64, u64, Vec<u8>);
#[derive(Debug, Clone)]
struct RawRecord {
    key: Vec<u8>,
    value: Vec<u8>,
    seq: u64,
    state: KeyState,
}

struct Best<T> {
    seq: u64,
    state: KeyState,
    value: T,
}

fn keep_newest<K: std::hash::Hash + Eq, T>(map: &mut HashMap<K, Best<T>>, key: K, seq: u64, state: KeyState, value: T) {
    match map.get_mut(&key) {
        Some(cur) if seq < cur.seq => {}
        Some(cur) => {
            cur.seq = seq;
            cur.state = state;
            cur.value = value;
        }
        None => {
            map.insert(key, Best { seq, state, value });
        }
    }
}

pub struct TeamsCache {
    _tmp: tempfile::TempDir,
    blob_dir: Option<PathBuf>,
    db_ids: HashMap<String, u64>,
    store_ids: HashMap<u64, HashMap<String, u64>>,
    store_records: HashMap<(u64, u64), Vec<RawRecord>>,
    blob_lookup: HashMap<(u64, u64, Vec<u8>, u64), BlobInfo>,
    pub bad_records: usize,
}

fn copy_dir_recursive(src: &Path, dst: &Path, skip_lock: bool) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        if skip_lock && name == "LOCK" {
            continue;
        }
        let sp = entry.path();
        let dp = dst.join(&name);
        let ft = entry.file_type()?;
        if ft.is_dir() {
            copy_dir_recursive(&sp, &dp, false)?;
        } else if ft.is_file() {
            fs::copy(&sp, &dp)?;
        }
    }
    Ok(())
}

impl TeamsCache {
    pub fn open<F>(source_dir: Option<&Path>, on_copied: Option<F>) -> Result<Self, SourceError>
    where
        F: FnOnce() -> Result<(), SourceError>,
    {
        let src = source_dir.map(PathBuf::from).unwrap_or_else(paths::source_dir);
        let ldb = src.join(format!("{}.leveldb", paths::ORIGIN));
        let blob = src.join(format!("{}.blob", paths::ORIGIN));
        if !ldb.is_dir() {
            return Err(SourceError(format!(
                "Teams cache not found at {}. Is the new Microsoft Teams installed and signed in?",
                ldb.display()
            )));
        }
        let tmp = tempfile::Builder::new()
            .prefix("teams-cli-")
            .tempdir()
            .map_err(|e| SourceError(format!("failed creating tempdir: {e}")))?;
        let _ = fs::set_permissions(tmp.path(), std::os::unix::fs::PermissionsExt::from_mode(0o700));
        let ldb_copy = tmp.path().join(ldb.file_name().unwrap());
        copy_dir_recursive(&ldb, &ldb_copy, true)
            .map_err(|e| SourceError(format!("copying LevelDB snapshot failed: {e}")))?;
        let blob_copy = if blob.is_dir() {
            let bc = tmp.path().join(blob.file_name().unwrap());
            copy_dir_recursive(&blob, &bc, false)
                .map_err(|e| SourceError(format!("copying blob snapshot failed: {e}")))?;
            Some(bc)
        } else {
            None
        };

        if let Some(cb) = on_copied {
            cb()?;
        }

        let mut db_meta_best: HashMap<Vec<u8>, Best<Vec<u8>>> = HashMap::new();
        let mut os_name_best: HashMap<(u64, u64), Best<Vec<u8>>> = HashMap::new();
        let mut store_records: HashMap<(u64, u64), Vec<RawRecord>> = HashMap::new();
        let mut blob_best: HashMap<BlobKey, Best<Vec<BlobInfo>>> = HashMap::new();

        leveldb::for_each_record(&ldb_copy, |k, val, seq, state| {
            if k.len() >= 5 && k[0..4] == [0, 0, 0, 0] && k[4] == 0xc9 {
                keep_newest(&mut db_meta_best, k.to_vec(), seq, state, val.to_vec());
                return;
            }
            let Some((db_id, store_id, index_id, pfx_len)) = read_prefix(k) else {
                return;
            };
            let suffix = &k[pfx_len..];
            if store_id == 0 && index_id == 0 {
                if !suffix.is_empty() && suffix[0] == 50 {
                    let mut p = 1usize;
                    if let Some(os_id) = read_varint(suffix, &mut p) {
                        if p < suffix.len() && suffix[p] == 0 {
                            keep_newest(&mut os_name_best, (db_id, os_id), seq, state, val.to_vec());
                        }
                    }
                }
            } else if db_id > 0 && store_id > 0 && index_id == 1 {
                if state != KeyState::Deleted {
                    store_records.entry((db_id, store_id)).or_default().push(RawRecord {
                        key: suffix.to_vec(),
                        value: val.to_vec(),
                        seq,
                        state,
                    });
                }
            } else if db_id > 0 && store_id > 0 && index_id == 3 {
                let infos = if state == KeyState::Deleted {
                    Vec::new()
                } else {
                    parse_blob_infos(val)
                };
                keep_newest(&mut blob_best, (db_id, store_id, suffix.to_vec()), seq, state, infos);
            }
        })
        .map_err(SourceError)?;

        Ok(Self::finish_index(
            tmp,
            blob_copy,
            db_meta_best,
            os_name_best,
            store_records,
            blob_best,
        ))
    }

    fn finish_index(
        tmp: tempfile::TempDir,
        blob_dir: Option<PathBuf>,
        db_meta_best: HashMap<Vec<u8>, Best<Vec<u8>>>,
        os_name_best: HashMap<(u64, u64), Best<Vec<u8>>>,
        mut store_records: HashMap<(u64, u64), Vec<RawRecord>>,
        blob_best: HashMap<BlobKey, Best<Vec<BlobInfo>>>,
    ) -> Self {
        for entries in store_records.values_mut() {
            entries.sort_by_key(|r| r.seq);
        }

        let mut blob_lookup: HashMap<(u64, u64, Vec<u8>, u64), BlobInfo> = HashMap::new();
        for ((db_id, store_id, suffix), best) in blob_best {
            if best.state == KeyState::Deleted {
                continue;
            }
            for (idx, info) in best.value.into_iter().enumerate() {
                blob_lookup.insert((db_id, store_id, suffix.clone(), idx as u64), info);
            }
        }

        let mut db_ids: HashMap<String, u64> = HashMap::new();
        for (key, best) in db_meta_best {
            if best.state == KeyState::Deleted {
                continue;
            }
            let val = best.value;
            let mut p = 5usize;
            let Some(orig_bytes) = read_varint(&key, &mut p)
                .and_then(|v| usize::try_from(v).ok())
                .and_then(|n| n.checked_mul(2))
            else {
                continue;
            };
            let Some(next_p) = p.checked_add(orig_bytes) else {
                continue;
            };
            p = next_p;
            if p > key.len() {
                continue;
            }
            let Some(name_bytes) = read_varint(&key, &mut p)
                .and_then(|v| usize::try_from(v).ok())
                .and_then(|n| n.checked_mul(2))
            else {
                continue;
            };
            let Some(name_end) = p.checked_add(name_bytes) else {
                continue;
            };
            if name_end > key.len() {
                continue;
            }
            let db_name = decode_utf16be(&key[p..name_end]);
            if let Some(id_no) = decode_truncated_int(&val) {
                db_ids.insert(db_name, id_no);
            }
        }

        let mut store_ids: HashMap<u64, HashMap<String, u64>> = HashMap::new();
        for ((db_id, os_id), best) in os_name_best {
            if best.state == KeyState::Deleted {
                continue;
            }
            let name = decode_utf16be(&best.value);
            store_ids.entry(db_id).or_default().insert(name, os_id);
        }

        Self {
            _tmp: tmp,
            blob_dir,
            db_ids,
            store_ids,
            store_records,
            blob_lookup,
            bad_records: 0,
        }
    }

    pub fn database_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.db_ids.keys().cloned().collect();
        names.sort();
        names
    }

    pub fn stores(&self, db_name: &str) -> Vec<String> {
        let Some(&db_id) = self.db_ids.get(db_name) else {
            return Vec::new();
        };
        let Some(map) = self.store_ids.get(&db_id) else {
            return Vec::new();
        };
        let mut out: Vec<String> = map.keys().cloned().collect();
        out.sort();
        out
    }

    pub fn find_database(&self, manager: &str) -> Option<String> {
        let pfx = format!("Teams:{manager}:");
        let mut candidates: Vec<&String> = self.db_ids.keys().filter(|n| n.starts_with(&pfx)).collect();
        candidates.sort();
        if let Some(pref) = candidates.iter().find(|n| n.contains(":react-web-client:")) {
            return Some((*pref).clone());
        }
        candidates.first().map(|s| (*s).clone())
    }

    pub fn user_object_id(&self) -> Option<String> {
        let mut names: Vec<&String> = self.db_ids.keys().collect();
        names.sort();
        for n in names {
            if let Some(caps) = USER_OID_RE.captures(n) {
                return Some(caps[1].to_ascii_lowercase());
            }
        }
        None
    }

    fn decode_blink_value(
        &self,
        decoder: &mut snap::raw::Decoder,
        db_id: u64,
        store_id: u64,
        raw_key: &[u8],
        buf: &[u8],
    ) -> Result<Value, String> {
        self.decode_blink_value_depth(decoder, db_id, store_id, raw_key, buf, 0)
    }

    fn decode_blink_value_depth(
        &self,
        decoder: &mut snap::raw::Decoder,
        db_id: u64,
        store_id: u64,
        raw_key: &[u8],
        buf: &[u8],
        depth: usize,
    ) -> Result<Value, String> {
        if depth >= MAX_DEPTH {
            return Err("Blink wrapper nesting depth exceeded".to_string());
        }
        if buf.is_empty() {
            return Ok(Value::Null);
        }
        let mut pos = 0usize;
        if buf[pos] != 0xFF {
            return Err("missing Blink 0xFF tag".to_string());
        }
        pos += 1;
        let blink_ver = read_varint(buf, &mut pos).ok_or("truncated Blink version")?;
        if blink_ver == 0x11 {
            if pos >= buf.len() {
                return Err("truncated Blink 0x11 wrapper".to_string());
            }
            let wrap_tag = buf[pos];
            pos += 1;
            if wrap_tag == 0x01 {
                let _size = read_varint(buf, &mut pos).ok_or("truncated blob size")?;
                let blob_idx = read_varint(buf, &mut pos).ok_or("truncated blob index")?;
                let info = self
                    .blob_lookup
                    .get(&(db_id, store_id, raw_key.to_vec(), blob_idx))
                    .ok_or("missing external blob info")?;
                let blob_root = self.blob_dir.as_ref().ok_or("blob dir not present")?;
                let path = blob_root
                    .join(format!("{db_id:x}"))
                    .join(format!("{:02x}", info.blob_number >> 8))
                    .join(format!("{:x}", info.blob_number));
                let blob_bytes = fs::read(&path).map_err(|e| format!("reading blob {}: {e}", path.display()))?;
                return self.decode_blink_value_depth(decoder, db_id, store_id, raw_key, &blob_bytes, depth + 1);
            } else if wrap_tag == 0x02 {
                let decompressed = decoder
                    .decompress_vec(&buf[pos..])
                    .map_err(|e| format!("snappy blink decompress: {e}"))?;
                return self.decode_blink_value_depth(decoder, db_id, store_id, raw_key, &decompressed, depth + 1);
            } else {
                return Err(format!("unknown Blink wrapper tag 0x{wrap_tag:02x}"));
            }
        }
        if blink_ver >= BLINK_MIN_TRAILER_VERSION {
            if pos + BLINK_TRAILER_SIZE > buf.len() || buf[pos] != BLINK_TRAILER_TAG {
                return Err("invalid Blink trailer".to_string());
            }
            pos += BLINK_TRAILER_SIZE;
        }
        Deserializer::deserialize(&buf[pos..])
    }

    fn decode_entries(
        &self,
        db_id: u64,
        store_id: u64,
        entries: &[RawRecord],
        mut cb: impl FnMut(Value, Value),
    ) -> usize {
        let mut decoder = snap::raw::Decoder::new();
        let mut bad = 0usize;
        for rec in entries {
            if rec.state == KeyState::Deleted {
                continue;
            }
            let (key_suffix, raw_val) = (&rec.key, &rec.value);
            let Ok((idb_key, _)) = IdbKey::parse(key_suffix) else {
                bad += 1;
                continue;
            };
            if raw_val.is_empty() {
                cb(idb_key.value, Value::Null);
                continue;
            }
            let mut pos = 0usize;
            if read_varint(raw_val, &mut pos).is_none() {
                bad += 1;
                continue;
            }
            match self.decode_blink_value(&mut decoder, db_id, store_id, &idb_key.raw_key, &raw_val[pos..]) {
                Ok(v) => cb(idb_key.value, v),
                Err(_) => bad += 1,
            }
        }
        bad
    }

    fn resolve_keyed_db(&self, manager: &str, store: &str) -> Option<(u64, u64)> {
        let pfx = format!("Teams:{manager}:");
        let mut names: Vec<&String> = self
            .db_ids
            .keys()
            .filter(|n| n.starts_with(&pfx) || *n == manager)
            .collect();
        names.sort_by(|a, b| {
            let a_pref = !a.contains(":react-web-client:");
            let b_pref = !b.contains(":react-web-client:");
            a_pref.cmp(&b_pref).then_with(|| a.cmp(b))
        });
        for name in names {
            if let Some(&db_id) = self.db_ids.get(name) {
                if let Some(&store_id) = self.store_ids.get(&db_id).and_then(|m| m.get(store)) {
                    return Some((db_id, store_id));
                }
            }
        }
        None
    }

    pub fn retain_wanted_stores(&mut self, direct: &[(&str, &str)], keyed: &[(&str, &str)]) {
        let mut keep = std::collections::HashSet::new();
        for &(manager, store) in direct {
            if let Some(db_name) = self.find_database(manager) {
                if let Some(&db_id) = self.db_ids.get(&db_name) {
                    if let Some(&store_id) = self.store_ids.get(&db_id).and_then(|m| m.get(store)) {
                        keep.insert((db_id, store_id));
                    }
                }
            }
        }
        for &(manager, store) in keyed {
            if let Some(pair) = self.resolve_keyed_db(manager, store) {
                keep.insert(pair);
            }
        }
        self.store_records.retain(|pair, _| keep.contains(pair));
        self.blob_lookup
            .retain(|(db_id, store_id, _, _), _| keep.contains(&(*db_id, *store_id)));
    }

    pub fn for_each_taken_record(&mut self, manager: &str, store: &str, mut cb: impl FnMut(Value)) {
        let Some(db_name) = self.find_database(manager) else {
            return;
        };
        let Some(&db_id) = self.db_ids.get(&db_name) else {
            return;
        };
        let Some(&store_id) = self.store_ids.get(&db_id).and_then(|m| m.get(store)) else {
            return;
        };
        let Some(entries) = self.store_records.remove(&(db_id, store_id)) else {
            return;
        };
        let bad = self.decode_entries(db_id, store_id, &entries, |_, v| cb(v));
        self.bad_records += bad;
    }

    pub fn for_each_taken_keyed_record(&mut self, manager: &str, store: &str, cb: impl FnMut(Value, Value)) {
        let Some((db_id, store_id)) = self.resolve_keyed_db(manager, store) else {
            return;
        };
        let Some(entries) = self.store_records.remove(&(db_id, store_id)) else {
            return;
        };
        let bad = self.decode_entries(db_id, store_id, &entries, cb);
        self.bad_records += bad;
    }

    pub fn raw_records_with_keys(&mut self, db_name: &str, store: &str) -> Vec<(Value, Value)> {
        let Some(&db_id) = self.db_ids.get(db_name) else {
            return Vec::new();
        };
        let Some(&store_id) = self.store_ids.get(&db_id).and_then(|m| m.get(store)) else {
            return Vec::new();
        };
        let Some(entries) = self.store_records.get(&(db_id, store_id)) else {
            return Vec::new();
        };
        let mut out = Vec::with_capacity(entries.len());
        let bad = self.decode_entries(db_id, store_id, entries, |k, v| out.push((k, v)));
        self.bad_records += bad;
        out
    }

    pub fn records(&mut self, manager: &str, store: &str) -> Vec<Value> {
        let Some(db_name) = self.find_database(manager) else {
            return Vec::new();
        };
        self.raw_records_with_keys(&db_name, store)
            .into_iter()
            .map(|(_, v)| v)
            .collect()
    }

    pub fn keyed_records(&mut self, manager: &str, store: &str) -> Vec<(Value, Value)> {
        let Some((db_id, store_id)) = self.resolve_keyed_db(manager, store) else {
            return Vec::new();
        };
        let Some(entries) = self.store_records.get(&(db_id, store_id)) else {
            return Vec::new();
        };
        let mut out = Vec::with_capacity(entries.len());
        let bad = self.decode_entries(db_id, store_id, entries, |k, v| out.push((k, v)));
        self.bad_records += bad;
        out
    }
}
