use crate::ingest::leveldb::read_varint;
use serde_json::{Map, Number, Value};

const K_VERSION: u8 = 0xFF;
const K_PADDING: u8 = 0x00;
const K_VERIFY_OBJECT_COUNT: u8 = b'?';
const K_THE_HOLE: u8 = b'-';
const K_UNDEFINED: u8 = b'_';
const K_NULL: u8 = b'0';
const K_TRUE: u8 = b'T';
const K_FALSE: u8 = b'F';
const K_INT32: u8 = b'I';
const K_UINT32: u8 = b'U';
const K_DOUBLE: u8 = b'N';
const K_BIGINT: u8 = b'Z';
const K_UTF8_STRING: u8 = b'S';
const K_ONE_BYTE_STRING: u8 = b'"';
const K_TWO_BYTE_STRING: u8 = b'c';
const K_OBJECT_REFERENCE: u8 = b'^';
const K_BEGIN_JS_OBJECT: u8 = b'o';
const K_END_JS_OBJECT: u8 = b'{';
const K_BEGIN_SPARSE_JS_ARRAY: u8 = b'a';
const K_END_SPARSE_JS_ARRAY: u8 = b'@';
const K_BEGIN_DENSE_JS_ARRAY: u8 = b'A';
const K_END_DENSE_JS_ARRAY: u8 = b'$';
const K_DATE: u8 = b'D';
const K_TRUE_OBJECT: u8 = b'y';
const K_FALSE_OBJECT: u8 = b'x';
const K_NUMBER_OBJECT: u8 = b'n';
const K_BIGINT_OBJECT: u8 = b'z';
const K_STRING_OBJECT: u8 = b's';
const K_REGEXP: u8 = b'R';
const K_BEGIN_JS_MAP: u8 = b';';
const K_END_JS_MAP: u8 = b':';
const K_BEGIN_JS_SET: u8 = b'\'';
const K_END_JS_SET: u8 = b',';
const K_ARRAY_BUFFER: u8 = b'B';
const K_ARRAY_BUFFER_VIEW: u8 = b'V';
const K_SHARED_OBJECT: u8 = b'p';
const K_HOST_OBJECT: u8 = b'\\';

const MAX_DEPTH: usize = 128;
const MAX_SPARSE_LEN: usize = 1_000_000;

pub struct Deserializer<'a> {
    buf: &'a [u8],
    pos: usize,
    version: u64,
    depth: usize,
    objects: Vec<Value>,
}

impl<'a> Deserializer<'a> {
    pub fn deserialize(buf: &'a [u8]) -> Result<Value, String> {
        let mut d = Self {
            buf,
            pos: 0,
            version: 0,
            depth: 0,
            objects: Vec::with_capacity(32),
        };
        let tag = d.read_tag()?;
        if tag != K_VERSION {
            return Err(format!("expected V8 version tag 0xFF, got 0x{tag:02x}"));
        }
        d.version = d.read_varint()?;
        if d.peek_tag() == Some(K_VERIFY_OBJECT_COUNT) {
            let _ = d.read_tag()?;
            let _ = d.read_varint()?;
        }
        d.read_object()
    }

    fn read_raw(&mut self, len: usize) -> Result<&'a [u8], String> {
        let end = self
            .pos
            .checked_add(len)
            .ok_or_else(|| "overflow in read_raw".to_string())?;
        if end > self.buf.len() {
            return Err(format!(
                "truncated V8 stream at {}: wanted {len}, have {}",
                self.pos,
                self.buf.len() - self.pos
            ));
        }
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn read_varint(&mut self) -> Result<u64, String> {
        read_varint(self.buf, &mut self.pos).ok_or_else(|| "truncated varint".to_string())
    }

    fn read_tag(&mut self) -> Result<u8, String> {
        while self.pos < self.buf.len() {
            let b = self.buf[self.pos];
            self.pos += 1;
            if b != K_PADDING {
                return Ok(b);
            }
        }
        Err("unexpected EOF reading V8 tag".to_string())
    }

    fn peek_tag(&self) -> Option<u8> {
        let mut p = self.pos;
        while p < self.buf.len() {
            let b = self.buf[p];
            if b != K_PADDING {
                return Some(b);
            }
            p += 1;
        }
        None
    }

    fn read_double(&mut self) -> Result<f64, String> {
        let b = self.read_raw(8)?;
        Ok(f64::from_le_bytes(b.try_into().unwrap()))
    }

    fn read_zigzag(&mut self) -> Result<i64, String> {
        let u = self.read_varint()?;
        Ok(if (u & 1) != 0 {
            -((u >> 1) as i64) - 1
        } else {
            (u >> 1) as i64
        })
    }

    fn read_len(&mut self) -> Result<usize, String> {
        let v = self.read_varint()?;
        usize::try_from(v).map_err(|_| "length overflow".to_string())
    }

    fn read_utf8_string(&mut self) -> Result<String, String> {
        let len = self.read_len()?;
        let raw = self.read_raw(len)?;
        Ok(String::from_utf8_lossy(raw).into_owned())
    }

    fn read_one_byte_string(&mut self) -> Result<String, String> {
        let len = self.read_len()?;
        let raw = self.read_raw(len)?;
        if raw.is_ascii() {
            Ok(std::str::from_utf8(raw)
                .map_err(|e| format!("invalid ASCII string: {e}"))?
                .to_string())
        } else {
            let mut s = String::with_capacity(raw.len() + 8);
            for &b in raw {
                s.push(b as char);
            }
            Ok(s)
        }
    }

    fn read_two_byte_string(&mut self) -> Result<String, String> {
        let byte_len = self.read_len()?;
        let raw = self.read_raw(byte_len)?;
        let units: Vec<u16> = raw.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
        Ok(String::from_utf16_lossy(&units))
    }

    fn read_string(&mut self) -> Result<String, String> {
        if self.version < 12 {
            return self.read_utf8_string();
        }
        match self.read_object()? {
            Value::String(s) => Ok(s),
            other => Ok(value_to_key_string(&other)),
        }
    }

    fn read_bigint(&mut self) -> Result<Value, String> {
        let bitfield = self.read_varint()?;
        let is_neg = (bitfield & 1) != 0;
        let byte_len = usize::try_from(bitfield >> 1).map_err(|_| "BigInt length overflow".to_string())?;
        let rem = self.buf.len().saturating_sub(self.pos);
        if byte_len > rem {
            return Err(format!("truncated V8 BigInt: wanted {byte_len} bytes, have {rem}"));
        }
        let raw = self.read_raw(byte_len)?;
        Ok(bigint_to_json(raw, is_neg))
    }

    fn read_host_object(&mut self) -> Result<Value, String> {
        let tag = self.read_raw(1)?[0];
        let val = match tag {
            b'i' | b'e' => {
                let idx = self.read_varint()?;
                Value::String(format!("BlobIndex({idx})"))
            }
            b'L' => {
                let len = self.read_len()?;
                let rem = self.buf.len().saturating_sub(self.pos);
                if len > rem {
                    return Err("truncated Blink file list".to_string());
                }
                let mut arr = Vec::with_capacity(len);
                for _ in 0..len {
                    let idx = self.read_varint()?;
                    arr.push(Value::String(format!("FileIndex({idx})")));
                }
                Value::Array(arr)
            }
            b'n' | b'N' => {
                let name = self.read_utf8_string()?;
                let _tok = self.read_varint()?;
                Value::String(name)
            }
            _ => return Err(format!("unsupported Blink host object tag 0x{tag:02x}")),
        };
        self.objects.push(val.clone());
        Ok(val)
    }

    fn read_object(&mut self) -> Result<Value, String> {
        if self.depth >= MAX_DEPTH {
            return Err("V8 nesting depth exceeded".to_string());
        }
        self.depth += 1;
        let is_array_buffer = self.peek_tag() == Some(K_ARRAY_BUFFER);
        let res = self.read_object_internal().and_then(|mut val| {
            if is_array_buffer && self.peek_tag() == Some(K_ARRAY_BUFFER_VIEW) {
                let _ = self.read_tag()?;
                val = self.wrap_array_buffer_view(val)?;
            }
            Ok(val)
        });
        self.depth -= 1;
        res
    }

    fn wrap_array_buffer_view(&mut self, raw: Value) -> Result<Value, String> {
        let _subtag = self.read_varint()?;
        let _byte_offset = self.read_varint()?;
        let _byte_length = self.read_varint()?;
        if self.version >= 14 {
            let _flags = self.read_varint()?;
        }
        self.objects.push(raw.clone());
        Ok(raw)
    }

    fn read_object_internal(&mut self) -> Result<Value, String> {
        let tag = self.read_tag()?;
        match tag {
            K_UNDEFINED | K_THE_HOLE | K_NULL => Ok(Value::Null),
            K_TRUE => Ok(Value::Bool(true)),
            K_FALSE => Ok(Value::Bool(false)),
            K_INT32 => Ok(Value::Number(self.read_zigzag()?.into())),
            K_UINT32 => Ok(Value::Number(self.read_varint()?.into())),
            K_DOUBLE => Ok(f64_to_json(self.read_double()?)),
            K_BIGINT => self.read_bigint(),
            K_UTF8_STRING => Ok(Value::String(self.read_utf8_string()?)),
            K_ONE_BYTE_STRING => Ok(Value::String(self.read_one_byte_string()?)),
            K_TWO_BYTE_STRING => Ok(Value::String(self.read_two_byte_string()?)),
            K_TRUE_OBJECT => {
                self.objects.push(Value::Bool(true));
                Ok(Value::Bool(true))
            }
            K_FALSE_OBJECT => {
                self.objects.push(Value::Bool(false));
                Ok(Value::Bool(false))
            }
            K_NUMBER_OBJECT => {
                let v = f64_to_json(self.read_double()?);
                self.objects.push(v.clone());
                Ok(v)
            }
            K_BIGINT_OBJECT => {
                let v = self.read_bigint()?;
                self.objects.push(v.clone());
                Ok(v)
            }
            K_STRING_OBJECT => {
                let v = Value::String(self.read_string()?);
                self.objects.push(v.clone());
                Ok(v)
            }
            K_DATE => {
                let ms = self.read_double()?;
                let v = f64_to_json(ms);
                self.objects.push(v.clone());
                Ok(v)
            }
            K_REGEXP => {
                let pattern = self.read_string()?;
                let _flags = self.read_varint()?;
                let v = Value::String(pattern);
                self.objects.push(v.clone());
                Ok(v)
            }
            K_OBJECT_REFERENCE => {
                let id = usize::try_from(self.read_varint()?).map_err(|_| "object id overflow".to_string())?;
                Ok(self.objects.get(id).cloned().unwrap_or(Value::Null))
            }
            K_BEGIN_JS_OBJECT => {
                let slot = self.objects.len();
                self.objects.push(Value::Null);
                let mut map = Map::new();
                while self.peek_tag() != Some(K_END_JS_OBJECT) {
                    let k = self.read_object()?;
                    let v = self.read_object()?;
                    map.insert(value_to_key_string(&k), v);
                }
                let _ = self.read_tag()?;
                let _prop_count = self.read_varint()?;
                let val = Value::Object(map);
                self.objects[slot] = val.clone();
                Ok(val)
            }
            K_BEGIN_SPARSE_JS_ARRAY => {
                let length = self.read_len()?;
                let rem_at_start = self.buf.len().saturating_sub(self.pos);
                let slot = self.objects.len();
                self.objects.push(Value::Null);
                let mut entries: Vec<(usize, Value)> = Vec::new();
                while self.peek_tag() != Some(K_END_SPARSE_JS_ARRAY) {
                    let k = self.read_object()?;
                    let v = self.read_object()?;
                    if let Some(idx) = value_to_index(&k) {
                        if idx < length {
                            entries.push((idx, v));
                        }
                    }
                }
                let _ = self.read_tag()?;
                let _prop_count = self.read_varint()?;
                let _expected_len = self.read_varint()?;
                let cap = rem_at_start.max(entries.len()).min(MAX_SPARSE_LEN);
                let final_len = length.min(cap);
                let mut arr = vec![Value::Null; final_len];
                for (idx, v) in entries {
                    if idx < final_len {
                        arr[idx] = v;
                    }
                }
                let val = Value::Array(arr);
                self.objects[slot] = val.clone();
                Ok(val)
            }
            K_BEGIN_DENSE_JS_ARRAY => {
                let length = self.read_len()?;
                let rem = self.buf.len().saturating_sub(self.pos);
                if length > rem {
                    return Err("truncated dense JS array".to_string());
                }
                let slot = self.objects.len();
                self.objects.push(Value::Null);
                let mut arr = Vec::with_capacity(length.min(1_000_000));
                for _ in 0..length {
                    arr.push(self.read_object()?);
                }
                while self.peek_tag() != Some(K_END_DENSE_JS_ARRAY) {
                    let k = self.read_object()?;
                    let v = self.read_object()?;
                    if let Some(idx) = value_to_index(&k) {
                        if idx < arr.len() {
                            arr[idx] = v;
                        }
                    }
                }
                let _ = self.read_tag()?;
                let _prop_count = self.read_varint()?;
                let _expected_len = self.read_varint()?;
                let val = Value::Array(arr);
                self.objects[slot] = val.clone();
                Ok(val)
            }
            K_BEGIN_JS_MAP => {
                let slot = self.objects.len();
                self.objects.push(Value::Null);
                let mut map = Map::new();
                while self.peek_tag() != Some(K_END_JS_MAP) {
                    let k = self.read_object()?;
                    let v = self.read_object()?;
                    map.insert(value_to_key_string(&k), v);
                }
                let _ = self.read_tag()?;
                let _expected_len = self.read_varint()?;
                let val = Value::Object(map);
                self.objects[slot] = val.clone();
                Ok(val)
            }
            K_BEGIN_JS_SET => {
                let slot = self.objects.len();
                self.objects.push(Value::Null);
                let mut arr = Vec::new();
                while self.peek_tag() != Some(K_END_JS_SET) {
                    arr.push(self.read_object()?);
                }
                let _ = self.read_tag()?;
                let _expected_len = self.read_varint()?;
                let val = Value::Array(arr);
                self.objects[slot] = val.clone();
                Ok(val)
            }
            K_ARRAY_BUFFER => {
                let len = self.read_len()?;
                let raw = self.read_raw(len)?;
                let mut s = String::with_capacity(raw.len());
                for &b in raw {
                    s.push(b as char);
                }
                let val = Value::String(s);
                self.objects.push(val.clone());
                Ok(val)
            }
            K_SHARED_OBJECT => {
                let id = self.read_varint()?;
                Ok(Value::String(format!("<SharedObject; id: {id}>")))
            }
            K_HOST_OBJECT => self.read_host_object(),
            other => Err(format!("unknown V8 tag 0x{other:02x} at offset {}", self.pos - 1)),
        }
    }
}

fn bigint_to_json(raw: &[u8], is_neg: bool) -> Value {
    let sig = raw.iter().rposition(|&b| b != 0).map(|i| i + 1).unwrap_or(0);
    let mag = &raw[..sig];
    if mag.len() <= 8 {
        let mut u = 0u64;
        for (i, &b) in mag.iter().enumerate() {
            u |= (b as u64) << (i * 8);
        }
        if !is_neg {
            if let Ok(i) = i64::try_from(u) {
                return Value::Number(i.into());
            }
        } else if u <= (i64::MAX as u64) + 1 {
            let i = if u == (i64::MAX as u64) + 1 {
                i64::MIN
            } else {
                -(u as i64)
            };
            return Value::Number(i.into());
        }
    }
    let digits = le_bytes_to_decimal(mag);
    if is_neg && digits != "0" {
        Value::String(format!("-{digits}"))
    } else {
        Value::String(digits)
    }
}

fn le_bytes_to_decimal(le: &[u8]) -> String {
    let mut limbs: Vec<u32> = le
        .chunks(4)
        .rev()
        .map(|c| {
            let mut w = 0u32;
            for (i, &b) in c.iter().enumerate() {
                w |= (b as u32) << (i * 8);
            }
            w
        })
        .collect();
    while limbs.first() == Some(&0) {
        limbs.remove(0);
    }
    if limbs.is_empty() {
        return "0".to_string();
    }
    let mut groups: Vec<u32> = Vec::new();
    while !limbs.is_empty() {
        let mut rem: u64 = 0;
        for limb in limbs.iter_mut() {
            let cur = (rem << 32) | (*limb as u64);
            *limb = (cur / 1_000_000_000) as u32;
            rem = cur % 1_000_000_000;
        }
        groups.push(rem as u32);
        while limbs.first() == Some(&0) {
            limbs.remove(0);
        }
    }
    let mut out = groups.last().map(|g| g.to_string()).unwrap_or_default();
    for g in groups.iter().rev().skip(1) {
        out.push_str(&format!("{g:09}"));
    }
    out
}

fn f64_to_json(f: f64) -> Value {
    if !f.is_finite() {
        return Value::Null;
    }
    Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null)
}

fn value_to_key_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.to_string()
            } else if let Some(u) = n.as_u64() {
                u.to_string()
            } else {
                n.to_string()
            }
        }
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

fn value_to_index(v: &Value) -> Option<usize> {
    match v {
        Value::Number(n) => n.as_u64().map(|u| u as usize),
        Value::String(s) => s.parse::<usize>().ok(),
        _ => None,
    }
}
