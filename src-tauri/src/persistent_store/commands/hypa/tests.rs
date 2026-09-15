use super::*;
use serde_json::json;

fn frame(header: serde_json::Value, body: &[u8]) -> Vec<u8> {
    let encoded = serde_json::to_vec(&header).expect("serialize header");
    let mut bytes = (encoded.len() as u32).to_le_bytes().to_vec();
    bytes.extend_from_slice(&encoded);
    bytes.extend_from_slice(body);
    bytes
}

fn vector_bytes(values: &[f32]) -> Vec<u8> {
    values.iter().flat_map(|value| value.to_le_bytes()).collect()
}

fn write_entry(key: &str, dimensions: i64, byte_length: usize) -> serde_json::Value {
    json!({
        "key": key,
        "producer": "hypa-v2",
        "model": "MiniLM",
        "endpoint": null,
        "preprocessVersion": 1,
        "dimensions": dimensions,
        "byteLength": byte_length,
        "metadata": null,
    })
}

#[test]
fn a_write_frame_splits_the_vector_region_in_header_order() {
    let body = [vector_bytes(&[1.0, 2.0]), vector_bytes(&[3.0])].concat();
    let entries = decode_write_frame(&frame(
        json!({ "entries": [write_entry("key-a", 2, 8), write_entry("key-b", 1, 4)] }),
        &body,
    ))
    .expect("decode write frame");

    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].cache_key, "key-a");
    assert_eq!(entries[0].vector, vector_bytes(&[1.0, 2.0]));
    assert_eq!(entries[1].cache_key, "key-b");
    assert_eq!(entries[1].vector, vector_bytes(&[3.0]));
}

#[test]
fn a_write_frame_with_a_short_or_long_vector_region_is_rejected() {
    let header = json!({ "entries": [write_entry("key-a", 2, 8)] });
    assert!(decode_write_frame(&frame(header.clone(), &vector_bytes(&[1.0]))).is_err());
    assert!(decode_write_frame(&frame(header, &vector_bytes(&[1.0, 2.0, 3.0]))).is_err());
}

#[test]
fn a_write_frame_whose_byte_length_contradicts_the_dimensions_is_rejected() {
    assert!(decode_write_frame(&frame(
        json!({ "entries": [write_entry("key-a", 2, 4)] }),
        &vector_bytes(&[1.0]),
    ))
    .is_err());
}

#[test]
fn a_truncated_or_oversized_header_length_is_rejected() {
    assert!(decode_write_frame(&[0, 0, 0]).is_err());
    assert!(decode_write_frame(&[255, 255, 255, 255]).is_err());
}

#[test]
fn a_read_frame_reports_a_miss_as_an_empty_vector() {
    let built = build_frame(
        &ReadHeader {
            entries: vec![
                ReadHeaderEntry {
                    key: "key-a".to_owned(),
                    dimensions: 2,
                    byte_length: 8,
                },
                ReadHeaderEntry {
                    key: "missing".to_owned(),
                    dimensions: 0,
                    byte_length: 0,
                },
            ],
        },
        vec![vector_bytes(&[1.0, 2.0]), Vec::new()],
    )
    .expect("build read frame");

    let (header_bytes, body) = split_frame(&built).expect("split read frame");
    let header: serde_json::Value =
        serde_json::from_slice(header_bytes).expect("parse read header");
    assert_eq!(
        header,
        json!({ "entries": [
            { "key": "key-a", "dimensions": 2, "byteLength": 8 },
            { "key": "missing", "dimensions": 0, "byteLength": 0 },
        ]})
    );
    assert_eq!(body, vector_bytes(&[1.0, 2.0]));
}

#[test]
fn a_base64_json_body_decodes_to_the_same_frame_as_a_raw_body() {
    let body = vector_bytes(&[1.0, 2.0]);
    let bytes = frame(json!({ "entries": [write_entry("key-a", 2, 8)] }), &body);
    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);

    let decoded: EncodedWrite =
        serde_json::from_value(json!({ "payload": encoded })).expect("parse encoded write");
    let recovered = base64::engine::general_purpose::STANDARD
        .decode(decoded.payload.as_bytes())
        .expect("decode payload");
    assert_eq!(recovered, bytes);
    assert_eq!(
        decode_write_frame(&recovered).expect("decode write frame")[0].vector,
        body
    );
}
