use serde_json::Value;
use std::io::Read;
use teams_cli::json_codec;
use teams_cli::storage::archive;

const VECTORS: &[(&str, &str, &str, &str)] = &[
    (
        "call-history-manager/call-history",
        r#"{"id": "c1", "startTime": "2026-11-23T20:22:53.8010001Z", "originalArrivalTime": 1795465373801.0, "durationInMs": 125400, "nested": {"z": [1, 2.5, null, true], "a": "é – “quoted” \n tab\t"}, "big": 1e+16, "tiny": 1e-05}"#,
        "617311d72c7607fb1cf824cc67201986",
        r#"{"id":"c1","startTime":"2026-11-23T20:22:53.8010001Z","originalArrivalTime":1795465373801.0,"durationInMs":125400,"nested":{"z":[1,2.5,null,true],"a":"é – “quoted” \n tab\t"},"big":1e+16,"tiny":1e-05}"#,
    ),
    (
        "message",
        r#"{"properties": 1, "sendPipelineOAMState": 1, "clientArrivalTime": 1, "prioritizeImDisplayName": 1, "isConversationLastMessage": 1, "sendPipelineStatus": 1, "pendingEmotionOps": 1, "isConversationLastMessageSanitized": 1, "source": 1, "keep": "x", "n": 3.0}"#,
        "a43144e52e2f948fe8766ff0c9fd6871",
        r#"{"properties":1,"sendPipelineOAMState":1,"clientArrivalTime":1,"prioritizeImDisplayName":1,"isConversationLastMessage":1,"sendPipelineStatus":1,"pendingEmotionOps":1,"isConversationLastMessageSanitized":1,"source":1,"keep":"x","n":3.0}"#,
    ),
    (
        "message",
        r#"{"id": "m1", "content": "<p>hi</p>", "emoji": "🚀", "arr": [], "obj": {}}"#,
        "0b094f551359f4888b91db2f2439397f",
        r#"{"id":"m1","content":"<p>hi</p>","emoji":"🚀","arr":[],"obj":{}}"#,
    ),
];

#[test]
fn record_hashes_are_stable() {
    for (source, data, hash, _) in VECTORS {
        let v: Value = serde_json::from_str(data).unwrap();
        assert_eq!(archive::record_hash(source, &v), *hash, "{source}: {data}");
    }
}

#[test]
fn stored_record_json_is_stable() {
    for (_, data, _, compact) in VECTORS {
        let v: Value = serde_json::from_str(data).unwrap();
        assert_eq!(json_codec::to_compact(&v), *compact);
        assert_eq!(json_codec::to_string(&v), *data, "canonical JSON round trip");
        let mut text = String::new();
        flate2::read::ZlibDecoder::new(&archive::encode_record(&v)[..])
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, *compact);
    }
}

#[test]
fn streaming_record_hash_matches_stable_view_for_all_sources() {
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use teams_cli::ingest::extract::stable_view;

    let cases = [
        (
            "message",
            json!({
                "id": "m1",
                "clientArrivalTime": 123,
                "properties": {"isread": true, "views": [1], "subject": "hi"},
                "content": "<p>hello</p>"
            }),
        ),
        (
            "conversation",
            json!({
                "id": "19:abc@thread.v2",
                "version": 5,
                "properties": {"consumptionhorizon": "1;2;3", "custom": "keep"},
                "threadProperties": {"isRead": true, "topic": "Project Alpha"}
            }),
        ),
        (
            "person",
            json!({"mri": "8:orgid:1", "metadata": {"x": 1}, "displayName": "Ada"}),
        ),
        (
            "activity-manager/feed-items",
            json!({"id": "a1", "isRead": false, "version": 2}),
        ),
        (
            "consumption-horizon-manager/consumption-horizon-history",
            json!({"id": "h1", "expiryTime": 999}),
        ),
    ];

    for (src, val) in cases {
        let materialized = json_codec::to_sorted(&stable_view(src, &val));
        let digest = Sha256::digest(materialized.as_bytes());
        let expected = hex::encode(&digest[..16]);
        assert_eq!(archive::record_hash(src, &val), expected, "mismatch for {src}");
    }
}
