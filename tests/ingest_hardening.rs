use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use teams_cli::ingest::extract::{self, derive_name};
use teams_cli::ingest::indexeddb::{IdbKey, TeamsCache};
use teams_cli::ingest::leveldb::{self, KeyState};
use teams_cli::ingest::render::html_to_text;
use teams_cli::ingest::{sync, v8};
use teams_cli::json_codec;
use teams_cli::paths;

fn push_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let mut b = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            b |= 0x80;
        }
        out.push(b);
        if v == 0 {
            break;
        }
    }
}

fn push_utf16be(out: &mut Vec<u8>, s: &str) {
    for u in s.encode_utf16() {
        out.extend_from_slice(&u.to_be_bytes());
    }
}

fn idb_key_str(s: &str) -> Vec<u8> {
    let mut out = vec![1u8];
    push_varint(&mut out, s.encode_utf16().count() as u64);
    push_utf16be(&mut out, s);
    out
}

fn idb_prefix(db_id: u64, store_id: u64, index_id: u64) -> Vec<u8> {
    vec![0x00, db_id as u8, store_id as u8, index_id as u8]
}

fn v8_str(out: &mut Vec<u8>, s: &str) {
    if s.is_ascii() {
        out.push(b'"');
    } else {
        out.push(b'S');
    }
    push_varint(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

fn v8_val(out: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Null => out.push(b'0'),
        Value::Bool(true) => out.push(b'T'),
        Value::Bool(false) => out.push(b'F'),
        Value::Number(n) => {
            out.push(b'N');
            out.extend_from_slice(&n.as_f64().unwrap().to_le_bytes());
        }
        Value::String(s) => v8_str(out, s),
        Value::Array(arr) => {
            out.push(b'A');
            push_varint(out, arr.len() as u64);
            for item in arr {
                v8_val(out, item);
            }
            out.push(b'$');
            push_varint(out, 0);
            push_varint(out, arr.len() as u64);
        }
        Value::Object(map) => {
            out.push(b'o');
            for (k, val) in map {
                v8_str(out, k);
                v8_val(out, val);
            }
            out.push(b'{');
            push_varint(out, map.len() as u64);
        }
    }
}

fn encode_idb_value(v: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    push_varint(&mut out, 1);
    out.extend_from_slice(&[0xFF, 20, 0xFF, 15]);
    v8_val(&mut out, v);
    out
}

type Entry = (Vec<u8>, Vec<u8>, u64, bool);

fn write_leveldb_log(path: &Path, entries: &[Entry]) {
    let mut log = Vec::new();
    for (k, v, seq, live) in entries {
        let mut batch = Vec::new();
        batch.extend_from_slice(&seq.to_le_bytes());
        batch.extend_from_slice(&1u32.to_le_bytes());
        batch.push(u8::from(*live));
        push_varint(&mut batch, k.len() as u64);
        batch.extend_from_slice(k);
        if *live {
            push_varint(&mut batch, v.len() as u64);
            batch.extend_from_slice(v);
        }
        let used = log.len() % 32_768;
        if used + 7 + batch.len() > 32_768 {
            log.extend(std::iter::repeat_n(0u8, 32_768 - used));
        }
        log.extend_from_slice(&0u32.to_le_bytes());
        log.extend_from_slice(&(batch.len() as u16).to_le_bytes());
        log.push(1u8);
        log.extend_from_slice(&batch);
    }
    fs::write(path, log).unwrap();
}

fn write_ldb(path: &Path, entries: &[Entry]) {
    fn block(kvs: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
        let mut b = Vec::new();
        for (k, v) in kvs {
            push_varint(&mut b, 0);
            push_varint(&mut b, k.len() as u64);
            push_varint(&mut b, v.len() as u64);
            b.extend_from_slice(k);
            b.extend_from_slice(v);
        }
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&1u32.to_le_bytes());
        b
    }
    let kvs: Vec<(Vec<u8>, Vec<u8>)> = entries
        .iter()
        .map(|(k, v, seq, live)| {
            let mut ik = k.clone();
            ik.extend_from_slice(&((seq << 8) | u64::from(*live)).to_le_bytes());
            (ik, v.clone())
        })
        .collect();
    let mut file = Vec::new();
    let data = block(&kvs);
    let data_len = data.len() as u64;
    file.extend_from_slice(&data);
    file.extend_from_slice(&[0, 0, 0, 0, 0]);
    let mut handle = Vec::new();
    push_varint(&mut handle, 0);
    push_varint(&mut handle, data_len);
    let idx = block(&[(vec![0xff], handle)]);
    let idx_off = file.len() as u64;
    let idx_len = idx.len() as u64;
    file.extend_from_slice(&idx);
    file.extend_from_slice(&[0, 0, 0, 0, 0]);
    let mut footer = Vec::new();
    push_varint(&mut footer, 0);
    push_varint(&mut footer, 0);
    push_varint(&mut footer, idx_off);
    push_varint(&mut footer, idx_len);
    footer.resize(40, 0);
    footer.extend_from_slice(&0xdb47_7524_8b80_fb57u64.to_le_bytes());
    file.extend_from_slice(&footer);
    fs::write(path, file).unwrap();
}

fn db_meta_key(db_name: &str) -> Vec<u8> {
    let origin = "https_teams.microsoft.com_0";
    let mut k = vec![0, 0, 0, 0, 0xc9];
    push_varint(&mut k, origin.encode_utf16().count() as u64);
    push_utf16be(&mut k, origin);
    push_varint(&mut k, db_name.encode_utf16().count() as u64);
    push_utf16be(&mut k, db_name);
    k
}

fn db_meta(db_name: &str, db_id: u64, seq: u64) -> Entry {
    (db_meta_key(db_name), vec![db_id as u8], seq, true)
}

fn store_name_key(db_id: u64, store_id: u64) -> Vec<u8> {
    let mut k = idb_prefix(db_id, 0, 0);
    k.push(50);
    push_varint(&mut k, store_id);
    k.push(0);
    k
}

fn store_name(db_id: u64, store_id: u64, name: &str, seq: u64) -> Entry {
    let mut v = Vec::new();
    push_utf16be(&mut v, name);
    (store_name_key(db_id, store_id), v, seq, true)
}

fn data_key(db_id: u64, store_id: u64, key: &str) -> Vec<u8> {
    let mut k = idb_prefix(db_id, store_id, 1);
    k.extend_from_slice(&idb_key_str(key));
    k
}

fn record(db_id: u64, store_id: u64, key: &str, v: &Value, seq: u64) -> Entry {
    (data_key(db_id, store_id, key), encode_idb_value(v), seq, true)
}

fn tombstone(key: Vec<u8>, seq: u64) -> Entry {
    (key, Vec::new(), seq, false)
}

struct Src {
    _td: tempfile::TempDir,
    root: std::path::PathBuf,
    ldb: std::path::PathBuf,
}

fn source() -> Src {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("source");
    let ldb = root.join(format!("{}.leveldb", paths::ORIGIN));
    fs::create_dir_all(&ldb).unwrap();
    Src { _td: td, root, ldb }
}

const ME_OID: &str = "aaaaaaaa-0000-0000-0000-000000000001";
const ADA: &str = "8:orgid:bbbbbbbb-0000-0000-0000-000000000002";

fn db_name(manager: &str) -> String {
    format!("Teams:{manager}:react-web-client:8:orgid:{ME_OID}:en-us")
}

fn open(src: &Src) -> TeamsCache {
    TeamsCache::open(
        Some(&src.root),
        None::<fn() -> Result<(), teams_cli::ingest::SourceError>>,
    )
    .unwrap()
}

fn v8(bytes: &[u8]) -> Result<Value, String> {
    let mut buf = vec![0xFF, 15];
    buf.extend_from_slice(bytes);
    v8::Deserializer::deserialize(&buf)
}

#[test]
fn v8_object_references_are_zero_based_ids() {
    let mut b = vec![b'o'];
    v8_str(&mut b, "a");
    b.push(b'o');
    v8_str(&mut b, "x");
    b.extend_from_slice(&[b'I', 2]);
    b.extend_from_slice(&[b'{', 1]);
    v8_str(&mut b, "b");
    b.extend_from_slice(&[b'^', 1]);
    v8_str(&mut b, "self");
    b.extend_from_slice(&[b'^', 0]);
    b.extend_from_slice(&[b'{', 3]);
    let v = v8(&b).unwrap();
    assert_eq!(v["a"], json!({"x": 1}));
    assert_eq!(v["b"], json!({"x": 1}));
    assert!(v["self"].is_null());
}

#[test]
fn v8_array_buffer_view_gets_its_own_object_id() {
    let mut b = vec![b'A', 3];
    b.extend_from_slice(&[b'B', 2, b'h', b'i']);
    b.extend_from_slice(&[b'V', b'B', 0, 2, 0]);
    b.extend_from_slice(&[b's', b'"', 3, b's', b't', b'r']);
    b.extend_from_slice(&[b'^', 3]);
    b.extend_from_slice(&[b'$', 0, 3]);
    let v = v8(&b).unwrap();
    assert_eq!(v, json!(["hi", "str", "str"]));

    let mut b = vec![b'A', 2];
    b.extend_from_slice(&[b'B', 2, b'h', b'i']);
    b.extend_from_slice(&[b'V', b'B', 0, 2, 0]);
    b.extend_from_slice(&[b'^', 2]);
    b.extend_from_slice(&[b'$', 0, 2]);
    assert_eq!(v8(&b).unwrap(), json!(["hi", "hi"]));
}

#[test]
fn v8_file_list_and_sparse_array_lengths_are_bounded() {
    let mut b = vec![b'\\', b'L'];
    push_varint(&mut b, 1_000_000);
    let err = v8(&b).unwrap_err();
    assert!(err.contains("truncated"), "{err}");

    let mut b = vec![b'a'];
    push_varint(&mut b, 1_000_000);
    b.extend_from_slice(&[b'@', 0]);
    push_varint(&mut b, 1_000_000);
    let v = v8(&b).unwrap();
    let len = v.as_array().unwrap().len();
    assert!(len <= 16, "sparse array materialized {len} holes");

    let mut b = vec![b'a'];
    push_varint(&mut b, u32::MAX as u64);
    assert!(v8(&b).is_err());

    let mut b = vec![b'a', 3, b'I', 2];
    v8_str(&mut b, "x");
    b.extend_from_slice(&[b'@', 1, 3]);
    assert_eq!(v8(&b).unwrap(), json!([null, "x", null]));

    let mut b = vec![b'a', 2, b'I', 20];
    v8_str(&mut b, "x");
    b.extend_from_slice(&[b'@', 1, 2]);
    assert_eq!(v8(&b).unwrap(), json!([null, null]));
}

fn bigint(neg: bool, mag_le: &[u8]) -> Result<Value, String> {
    let mut b = vec![b'Z'];
    push_varint(&mut b, ((mag_le.len() as u64) << 1) | u64::from(neg));
    b.extend_from_slice(mag_le);
    v8(&b)
}

#[test]
fn v8_bigint_bitfield_sign_and_overflow() {
    let mut one = [0u8; 8];
    one[0] = 1;
    assert_eq!(bigint(false, &one).unwrap(), json!(1));
    assert_eq!(bigint(true, &one).unwrap(), json!(-1));
    assert_eq!(bigint(false, &[]).unwrap(), json!(0));

    let mut p63 = [0u8; 8];
    p63[7] = 0x80;
    assert_eq!(bigint(true, &p63).unwrap(), json!(i64::MIN));
    assert_eq!(bigint(false, &p63).unwrap(), json!("9223372036854775808"));

    assert_eq!(bigint(false, &[0xff; 8]).unwrap(), json!("18446744073709551615"));
    let mut p64 = [0u8; 16];
    p64[8] = 1;
    assert_eq!(bigint(false, &p64).unwrap(), json!("18446744073709551616"));
    assert_eq!(bigint(true, &p64).unwrap(), json!("-18446744073709551616"));

    let mut p128 = [0u8; 24];
    p128[16] = 1;
    assert_eq!(
        bigint(false, &p128).unwrap(),
        json!("340282366920938463463374607431768211456")
    );
    let mut m128 = [0u8; 24];
    m128[..16].fill(0xff);
    assert_eq!(
        bigint(true, &m128).unwrap(),
        json!("-340282366920938463463374607431768211455")
    );

    let mut max = [0u8; 16];
    max[..8].copy_from_slice(&i64::MAX.to_le_bytes());
    assert_eq!(bigint(false, &max).unwrap(), json!(i64::MAX));

    let mut b = vec![b'Z'];
    push_varint(&mut b, 4096 << 1);
    b.extend_from_slice(&[1, 2, 3]);
    let err = v8(&b).unwrap_err();
    assert!(err.contains("truncated"), "{err}");

    let mut b = vec![b'Z'];
    push_varint(&mut b, u64::MAX);
    assert!(v8(&b).is_err());
}

#[test]
fn idb_key_parse_rejects_deeply_nested_arrays() {
    let mut buf = Vec::new();
    for _ in 0..10_000 {
        buf.extend_from_slice(&[4, 1]);
    }
    buf.push(0);
    let err = IdbKey::parse(&buf).unwrap_err();
    assert!(err.contains("depth"), "{err}");

    let (k, used) = IdbKey::parse(&[4, 1, 4, 1, 0]).unwrap();
    assert_eq!(used, 5);
    assert_eq!(k.value, json!([[null]]));
}

#[test]
fn leveldb_records_are_ordered_by_seq_across_files() {
    let src = source();
    let k = data_key(1, 1, "k");
    write_leveldb_log(&src.ldb.join("000003.log"), &[(k.clone(), b"new".to_vec(), 900, true)]);
    write_ldb(&src.ldb.join("000007.ldb"), &[(k.clone(), b"old".to_vec(), 10, true)]);
    write_ldb(&src.ldb.join("000009.ldb"), &[(k.clone(), Vec::new(), 5, false)]);
    let recs = leveldb::read_all_records(&src.ldb).unwrap();
    let seqs: Vec<u64> = recs.iter().map(|r| r.seq).collect();
    assert_eq!(seqs, vec![5, 10, 900]);
    assert_eq!(recs[0].state, KeyState::Deleted);
    assert_eq!(recs[1].value, b"old");
    assert_eq!(recs[2].value, b"new");
}

#[test]
fn newer_records_win_regardless_of_file_number() {
    let src = source();
    let conv_db = db_name("conversation-manager");
    let newer = json!({"id": "c1", "version": 1.0, "threadProperties": {"topic": "Newer"}});
    let older = json!({"id": "c1", "version": 1.0, "threadProperties": {"topic": "Older"}});
    write_leveldb_log(
        &src.ldb.join("000002.log"),
        &[
            db_meta(&conv_db, 3, 1),
            store_name(3, 1, "conversations", 2),
            record(3, 1, "c1", &newer, 500),
        ],
    );
    write_ldb(&src.ldb.join("000010.ldb"), &[record(3, 1, "c1", &older, 20)]);
    let mut cache = open(&src);
    let vals = cache.records("conversation-manager", "conversations");
    let topics: Vec<&str> = vals
        .iter()
        .map(|v| v["threadProperties"]["topic"].as_str().unwrap())
        .collect();
    assert_eq!(topics, vec!["Older", "Newer"], "newest record must be yielded last");
}

#[test]
fn metadata_tombstones_shadow_older_live_entries() {
    let src = source();
    let gone = "Teams:gone-manager:react-web-client:x";
    let kept = db_name("conversation-manager");
    write_leveldb_log(
        &src.ldb.join("000002.log"),
        &[
            db_meta(gone, 4, 1),
            db_meta(&kept, 3, 2),
            store_name(3, 1, "conversations", 3),
            store_name(3, 2, "dropped-store", 4),
            tombstone(db_meta_key(gone), 50),
            tombstone(store_name_key(3, 2), 60),
        ],
    );
    write_ldb(&src.ldb.join("000020.ldb"), &[db_meta(gone, 4, 10)]);
    let cache = open(&src);
    assert_eq!(cache.database_names(), vec![kept.clone()]);
    assert_eq!(cache.stores(&kept), vec!["conversations".to_string()]);

    let src2 = source();
    write_leveldb_log(
        &src2.ldb.join("000002.log"),
        &[
            db_meta(gone, 4, 1),
            tombstone(db_meta_key(gone), 5),
            db_meta(gone, 6, 9),
        ],
    );
    assert_eq!(open(&src2).database_names(), vec![gone.to_string()]);
}

#[test]
fn deleted_blob_metadata_is_not_used() {
    let src = source();
    let db = db_name("replychain-manager");
    fs::create_dir_all(src.root.join(format!("{}.blob", paths::ORIGIN)).join("2/00")).unwrap();
    let raw_key = idb_key_str("rc");
    let mut meta_key = idb_prefix(2, 1, 3);
    meta_key.extend_from_slice(&raw_key);
    let mut meta_val = vec![0u8];
    push_varint(&mut meta_val, 7);
    push_varint(&mut meta_val, 0);
    push_varint(&mut meta_val, 10);
    let mut blob = vec![0xFF, 20, 0xFF, 15];
    v8_val(&mut blob, &json!({"stale": true}));
    fs::write(src.root.join(format!("{}.blob", paths::ORIGIN)).join("2/00/7"), &blob).unwrap();
    let mut dk = idb_prefix(2, 1, 1);
    dk.extend_from_slice(&raw_key);
    let mut dv = Vec::new();
    push_varint(&mut dv, 1);
    dv.extend_from_slice(&[0xFF, 0x11, 0x01]);
    push_varint(&mut dv, blob.len() as u64);
    push_varint(&mut dv, 0);
    write_leveldb_log(
        &src.ldb.join("000002.log"),
        &[
            db_meta(&db, 2, 1),
            store_name(2, 1, "replychains-2", 2),
            (meta_key.clone(), meta_val, 3, true),
            (dk, dv, 4, true),
            tombstone(meta_key, 9),
        ],
    );
    let mut cache = open(&src);
    assert!(cache.raw_records_with_keys(&db, "replychains-2").is_empty());
    assert_eq!(cache.bad_records, 1);
}

#[test]
fn data_tombstones_are_not_emitted_as_null() {
    let src = source();
    let db = db_name("call-history-manager");
    write_leveldb_log(
        &src.ldb.join("000002.log"),
        &[
            db_meta(&db, 5, 1),
            store_name(5, 1, "call-history", 2),
            record(5, 1, "call-1", &json!({"callId": "call-1"}), 3),
            tombstone(data_key(5, 1, "call-1"), 40),
            tombstone(data_key(5, 1, "call-2"), 41),
        ],
    );
    let mut cache = open(&src);
    let rows = cache.raw_records_with_keys(&db, "call-history");
    assert_eq!(rows, vec![(json!("call-1"), json!({"callId": "call-1"}))]);
    assert!(rows.iter().all(|(_, v)| !v.is_null()));
}

#[test]
fn blink_wrapper_recursion_is_bounded() {
    let src = source();
    let db = db_name("profiles");
    let mut inner = vec![0xFF, 20, 0xFF, 15];
    v8_val(&mut inner, &json!({"mri": "x"}));
    for _ in 0..200 {
        let mut w = vec![0xFF, 0x11, 0x02];
        w.extend_from_slice(&snap::raw::Encoder::new().compress_vec(&inner).unwrap());
        inner = w;
    }
    let mut v = Vec::new();
    push_varint(&mut v, 1);
    v.extend_from_slice(&inner);
    write_leveldb_log(
        &src.ldb.join("000002.log"),
        &[
            db_meta(&db, 1, 1),
            store_name(1, 1, "profiles", 2),
            (data_key(1, 1, "deep"), v, 3, true),
        ],
    );
    let mut cache = open(&src);
    assert!(cache.records("profiles", "profiles").is_empty());
    assert_eq!(cache.bad_records, 1);
}

#[test]
fn user_object_id_accepts_uppercase_hex() {
    let src = source();
    let upper = "AAAAAAAA-0000-0000-0000-00000000000F";
    write_leveldb_log(
        &src.ldb.join("000002.log"),
        &[db_meta(
            &format!("Teams:profiles:react-web-client:8:orgid:{upper}:en-us"),
            1,
            1,
        )],
    );
    assert_eq!(
        open(&src).user_object_id().as_deref(),
        Some("aaaaaaaa-0000-0000-0000-00000000000f")
    );
}

fn message(id: Value, conv: &str, content: &str) -> Value {
    json!({
        "id": id,
        "conversationId": conv,
        "version": 1.0,
        "originalArrivalTime": 1_795_000_000_000.0,
        "creator": ADA,
        "imDisplayName": "Ada",
        "messageType": "RichText/Html",
        "content": content,
        "properties": {}
    })
}

#[test]
fn extract_dedupes_messages_numeric_ids_and_skips_extra_store_tombstones() {
    let src = source();
    let conv = "19:chat@thread.v2";
    let rc = |mm: Value| json!({"conversationId": conv, "replyChainId": "1", "messageMap": mm});
    let dup = message(json!("1795000000001"), conv, "<p>hi</p>");
    let numeric = message(json!(4_500_000_000_000.0), conv, "<p>numeric</p>");
    write_leveldb_log(
        &src.ldb.join("000002.log"),
        &[
            db_meta(&db_name("replychain-manager"), 2, 1),
            db_meta(&db_name("call-history-manager"), 5, 2),
            store_name(2, 1, "replychains-2", 3),
            store_name(2, 2, "replychains", 4),
            store_name(5, 1, "call-history", 5),
            record(2, 1, "rc", &rc(json!({"a": dup, "n": numeric})), 6),
            record(2, 2, "rc", &rc(json!({"a": dup})), 7),
            record(5, 1, "call-1", &json!({"callId": "call-1"}), 8),
            tombstone(data_key(5, 1, "call-1"), 20),
            tombstone(data_key(5, 1, "call-9"), 21),
        ],
    );
    let mut cache = open(&src);
    let snap = extract::extract(&mut cache).unwrap();

    let msg_keys: Vec<&str> = snap
        .records
        .iter()
        .filter(|r| r.source == "message")
        .map(|r| r.key.as_str())
        .collect();
    assert_eq!(msg_keys.len(), 2, "{msg_keys:?}");
    assert!(msg_keys.contains(&"19:chat@thread.v2/1795000000001"));
    assert!(msg_keys.contains(&"19:chat@thread.v2/4500000000000"));
    assert_eq!(snap.messages.len(), 2);

    let calls: Vec<&extract::RawRecord> = snap
        .records
        .iter()
        .filter(|r| r.source == "call-history-manager/call-history")
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].data, json!({"callId": "call-1"}));
    assert!(snap.records.iter().all(|r| !r.data.is_null()));
}

#[test]
fn derive_name_excludes_me_by_identity_not_display_name() {
    let me = format!("8:orgid:{ME_OID}");
    let me_upper = format!("8:orgid:{}", ME_OID.to_ascii_uppercase());
    let namesake = "8:orgid:dddddddd-0000-0000-0000-000000000004".to_string();
    let names: HashMap<String, String> = [
        (me.clone(), "Sam Smith".to_string()),
        (namesake.clone(), "Sam Smith".to_string()),
        (ADA.to_string(), "Ada".to_string()),
    ]
    .into_iter()
    .collect();
    let name_of = |m: &str| -> String {
        names
            .get(m)
            .cloned()
            .or_else(|| (teams_cli::identity::oid(m) == ME_OID).then(|| "Sam Smith".to_string()))
            .unwrap_or_else(|| m.to_string())
    };

    let (n, fb) = derive_name(
        "19:x@thread.v2",
        &[me_upper.clone(), namesake, ADA.to_string()],
        &name_of,
        Some(&me),
    );
    assert_eq!((n.as_str(), fb), ("Ada, Sam Smith", false));

    let (n, fb) = derive_name("19:x@thread.v2", &[me_upper], &name_of, Some(&me));
    assert_eq!((n.as_str(), fb), ("Sam Smith (you)", false));

    let cid = format!("19:{}_bbbbbbbb-0000-0000-0000-000000000002@unq.gbl.spaces", ME_OID);
    assert_eq!(derive_name(&cid, &[], &name_of, Some(&me)).0, "Ada");

    assert_eq!(
        derive_name("19:y@thread.v2", &[], &name_of, None),
        ("19:y@thread.v2".to_string(), true)
    );
}

#[test]
fn canonical_sorted_json_orders_keys_deterministically() {
    let v = json!({
        "z": {"b": [{"d": 1, "c": 2.0}], "a": "é"},
        "y": null,
        "ä": [[], {}],
        "B": true
    });
    assert_eq!(
        json_codec::to_sorted(&v),
        r#"{"B": true, "y": null, "z": {"a": "é", "b": [{"c": 2.0, "d": 1}]}, "ä": [[], {}]}"#
    );
    assert_eq!(json_codec::to_sorted(&json!(1.0)), "1.0");
}

#[test]
fn stale_temp_dirs_are_cleaned_up() {
    let td = tempfile::tempdir().unwrap();
    let old = td.path().join("teams-cli-old");
    let fresh = td.path().join("teams-cli-fresh");
    let other = td.path().join("unrelated-old");
    let old_file = td.path().join("teams-cli-file");
    for d in [&old, &fresh, &other] {
        fs::create_dir_all(d.join("sub")).unwrap();
        fs::write(d.join("sub/x"), b"x").unwrap();
    }
    fs::write(&old_file, b"x").unwrap();
    let two_hours_ago = SystemTime::now() - Duration::from_secs(7200);
    for p in [&old, &other, &old_file] {
        fs::File::open(p).unwrap().set_modified(two_hours_ago).unwrap();
    }
    let removed = sync::cleanup_stale_temp_dirs(td.path(), Duration::from_secs(3600));
    assert_eq!(removed, 1);
    assert!(!old.exists());
    assert!(fresh.exists());
    assert!(other.exists());
    assert!(old_file.exists());
}

#[test]
fn missing_cache_is_not_retried() {
    let td = tempfile::tempdir().unwrap();
    let t0 = Instant::now();
    let err = sync::snapshot_source(td.path(), 4, 5).unwrap_err();
    assert!(err.0.contains("Teams cache not found"), "{err}");
    assert!(t0.elapsed() < Duration::from_secs(2), "missing cache was retried");
}

#[test]
fn torn_snapshot_on_final_attempt_fails_loudly() {
    let src = source();
    write_leveldb_log(&src.ldb.join("000002.log"), &[db_meta(&db_name("profiles"), 1, 1)]);
    fs::write(src.ldb.join("000003.blob"), vec![0xAB; 8 * 1024 * 1024]).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let ready = Arc::new(AtomicBool::new(false));
    let writer = {
        let stop = stop.clone();
        let ready = ready.clone();
        let path = src.ldb.join("MANIFEST-000001");
        std::thread::spawn(move || {
            let mut file = fs::OpenOptions::new().create(true).append(true).open(&path).unwrap();
            let mut i = 0u64;
            while !stop.load(Ordering::Relaxed) {
                i += 1;
                let _ = std::io::Write::write_all(&mut file, &i.to_le_bytes());
                let _ = std::io::Write::flush(&mut file);
                ready.store(true, Ordering::Release);
            }
        })
    };
    while !ready.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
    let res = sync::snapshot_source(&src.root, 2, 0);
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
    let err = res.expect_err("a torn snapshot must not be accepted");
    assert!(err.0.contains("consistent snapshot"), "{err}");
    assert!(err.0.contains("during the copy"), "{err}");
}

#[test]
fn notification_text_is_passed_as_argv() {
    let evil = "x\" & (do shell script \"touch /tmp/pwned\") & \"\nline2";
    let args = sync::notification_args(evil);
    assert_eq!(
        &args[..6],
        &[
            "-e",
            "on run argv",
            "-e",
            "display notification (item 1 of argv) with title \"teams-cli\"",
            "-e",
            "end run"
        ]
    );
    assert_eq!(args.len(), 7);
    assert_eq!(args[6], evil.replace('\n', " "));
    assert!(!args[..6].iter().any(|a| a.contains("pwned")));
    assert!(!sync::notification_args("-l JavaScript")[6].starts_with('-'));
}

fn render(html: &str) -> String {
    html_to_text(html, &HashMap::new())
}

#[test]
fn tag_end_scan_skips_gt_inside_quoted_attributes() {
    assert_eq!(
        render(r#"<p><a href="https://e.com" title="a -> b">link</a> done</p>"#),
        "link (https://e.com) done"
    );
    assert_eq!(render(r#"<p><img alt='x > y' src="i.png">ok</p>"#), "[image: x > y]ok");
    assert_eq!(render("<b>it's</b> fine"), "it's fine");
}

#[test]
fn unclosed_comment_keeps_rest_of_message() {
    let out = render("<p>hello</p><!-- never closed <p>world</p>");
    assert!(out.contains("hello"), "{out}");
    assert!(out.contains("world"), "{out}");
    assert_eq!(render("<p>a<!-- closed -->b</p>"), "ab");
}

#[test]
fn stray_closing_pre_is_ignored() {
    assert_eq!(render("<p>text</pre> more</p>"), "text more");
    assert_eq!(render("<pre>code</pre></pre>after"), "```\ncode\n```\nafter");
}

#[test]
fn links_and_mentions_are_scoped_to_their_blockquote_frame() {
    let out = render(r#"<p>before</p><blockquote><a href="https://x.com">inside</blockquote> after</a>"#);
    assert!(!out.contains("x.com"), "{out}");
    assert!(out.starts_with("before"), "{out}");
    assert!(out.contains("> inside"), "{out}");
    assert!(out.ends_with("after"), "{out}");

    let mentions: HashMap<String, String> = [("0".to_string(), ADA.to_string())].into_iter().collect();
    let html = concat!(
        r#"<span itemtype="http://schema.skype.com/Mention" itemid="0">Ada</span>"#,
        r#"<blockquote> <b></b> <span itemtype="http://schema.skype.com/Mention" itemid="0">Lovelace</span></blockquote>"#
    );
    let out = html_to_text(html, &mentions);
    assert!(out.contains("> @Lovelace"), "{out}");

    let html = concat!(
        r#"<span itemtype="http://schema.skype.com/Mention" itemid="0">Ada</span> "#,
        r#"<span itemtype="http://schema.skype.com/Mention" itemid="0">Lovelace</span>"#
    );
    assert_eq!(html_to_text(html, &mentions), "@Ada Lovelace");
}

#[test]
fn incremental_sync_applies_delta_skips_noise_and_falls_back_on_meta() {
    const GROUP: &str = "19:delta-room@thread.v2";
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("TEAMS_CLI_HOME", home.path());

    let src = source();
    let rdb = db_name("replychain-manager");
    let pdb = db_name("profiles");
    let cdb = db_name("conversation-manager");
    let noise_db = db_name("telemetry-noise");
    write_leveldb_log(
        &src.ldb.join("000002.log"),
        &[
            db_meta(&pdb, 1, 1),
            store_name(1, 1, "profiles", 2),
            record(1, 1, ADA, &json!({"mri": ADA, "displayName": "Ada Lovelace"}), 3),
            db_meta(&cdb, 2, 4),
            store_name(2, 1, "conversations", 5),
            record(
                2,
                1,
                GROUP,
                &json!({"id": GROUP, "type": "Chat", "threadProperties": {"topic": "Delta Room"}}),
                6,
            ),
            db_meta(&rdb, 3, 7),
            store_name(3, 1, "replychains-2", 8),
            record(
                3,
                1,
                "rc1",
                &json!({
                    "conversationId": GROUP,
                    "messageMap": {
                        "1": {
                            "id": "1",
                            "conversationId": GROUP,
                            "content": "<p>first message</p>",
                            "messageType": "RichText/Html",
                            "creator": ADA,
                            "version": 100,
                            "originalArrivalTime": 1_700_000_001_000i64
                        }
                    }
                }),
                9,
            ),
            db_meta(&noise_db, 4, 10),
            store_name(4, 1, "events", 11),
        ],
    );

    let full = sync::run_sync(Some(&src.root), false, false).unwrap();
    assert!(full.get("incremental").is_none(), "{full}");
    assert_eq!(full["messages_new"], 1);

    write_leveldb_log(
        &src.ldb.join("000003.log"),
        &[record(
            3,
            1,
            "rc2",
            &json!({
                "conversationId": GROUP,
                "messageMap": {
                    "2": {
                        "id": "2",
                        "conversationId": GROUP,
                        "content": "<p>second delta message</p>",
                        "messageType": "RichText/Html",
                        "creator": ADA,
                        "version": 200,
                        "originalArrivalTime": 1_700_000_002_000i64
                    }
                }
            }),
            20,
        )],
    );

    let inc = sync::run_sync(Some(&src.root), false, false).unwrap();
    assert_eq!(inc["incremental"], true, "{inc}");
    assert_eq!(inc["messages_new"], 1, "{inc}");

    let conn = teams_cli::storage::archive::connect_readonly(None).unwrap();
    let status = teams_cli::query::api::status(&conn).unwrap();
    assert_eq!(status["counts"]["messages"], 2);
    assert_eq!(status["counts"]["preservedAfterTeamsDropped"], 0);
    let sender: String = conn
        .query_row("SELECT sender_name FROM messages WHERE message_id = '2'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(sender, "Ada Lovelace");
    drop(conn);

    write_leveldb_log(
        &src.ldb.join("000004.log"),
        &[record(4, 1, "n1", &json!({"ping": true}), 30)],
    );
    let noise = sync::run_sync(Some(&src.root), false, false).unwrap();
    assert_eq!(noise["incremental"], true, "{noise}");
    assert!(noise.get("skipped").is_some(), "{noise}");

    write_leveldb_log(&src.ldb.join("000005.log"), &[store_name(4, 2, "more-events", 40)]);
    let fallback = sync::run_sync(Some(&src.root), false, false).unwrap();
    assert!(fallback.get("incremental").is_none(), "{fallback}");

    write_leveldb_log(
        &src.ldb.join("000006.log"),
        &[(data_key(3, 1, "bad-rec"), vec![1, 0x00, 0x00], 50, true)],
    );
    let bad_fallback = sync::run_sync(Some(&src.root), false, false).unwrap();
    assert!(bad_fallback.get("incremental").is_none(), "{bad_fallback}");

    let forced = sync::run_sync(Some(&src.root), false, true).unwrap();
    assert!(forced.get("incremental").is_none(), "{forced}");
}
